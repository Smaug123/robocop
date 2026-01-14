//! Webhook replay protection operations for SQLite repository.
//!
//! This module implements idempotent webhook processing with claim semantics:
//! - First request claims the webhook and processes it
//! - Concurrent requests see InProgress and return retryable errors
//! - After completion, requests see Completed and return success without reprocessing
//!
//! Stale claims (older than 30 minutes) can be reclaimed to handle server crashes.

use rusqlite::{params, Connection};

use super::super::{RepositoryError, WebhookClaimResult};
use super::SqliteRepository;

/// TTL for stale InProgress claims (30 minutes).
///
/// If a webhook claim is still InProgress after this duration, it's considered
/// abandoned (e.g., due to a crash or panic) and can be reclaimed. This prevents
/// permanent blocking of webhook IDs while being long enough that legitimate
/// slow handlers won't be interrupted.
///
/// 30 minutes is chosen because:
/// - Normal webhook processing should complete in seconds to minutes
/// - OpenAI's retry interval is likely several minutes
/// - Long enough to avoid race conditions with legitimate slow handlers
pub(super) const STALE_IN_PROGRESS_TTL_SECONDS: i64 = 30 * 60;

/// Check if a webhook ID was successfully processed (completed).
pub(super) fn is_webhook_seen_sync(conn: &Connection, webhook_id: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM seen_webhook_ids WHERE webhook_id = ?1 AND claim_state = 1)",
        params![webhook_id],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

/// Record a webhook ID as completed (for legacy record_webhook_id API).
pub(super) fn record_webhook_id_sync(
    conn: &Connection,
    webhook_id: &str,
    now_secs: i64,
) -> Result<(), String> {
    // claim_state: 1 = completed
    conn.execute(
        "INSERT OR REPLACE INTO seen_webhook_ids (webhook_id, recorded_at, claim_state) VALUES (?1, ?2, 1)",
        params![webhook_id, now_secs],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Atomically try to claim a webhook ID for processing.
pub(super) fn try_claim_webhook_id_sync(
    conn: &Connection,
    webhook_id: &str,
    now_secs: i64,
    stale_cutoff: i64,
) -> Result<WebhookClaimResult, String> {
    // Use atomic INSERT OR IGNORE to avoid the read-then-insert race condition.
    // If two processes both see "missing" and try to insert, the loser's insert
    // is silently ignored (no error), and we detect this via changes() == 0.
    conn.execute(
        "INSERT OR IGNORE INTO seen_webhook_ids (webhook_id, recorded_at, claim_state) VALUES (?1, ?2, 0)",
        params![webhook_id, now_secs],
    )
    .map_err(|e| e.to_string())?;

    if conn.changes() > 0 {
        // Insert succeeded - we claimed it
        return Ok(WebhookClaimResult::Claimed);
    }

    // Insert was ignored (row already exists) - check the existing state
    let (existing_state, recorded_at): (i64, i64) = conn
        .query_row(
            "SELECT claim_state, recorded_at FROM seen_webhook_ids WHERE webhook_id = ?1",
            params![webhook_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())?;

    match existing_state {
        0 => {
            // InProgress - check if stale (abandoned due to crash/panic)
            if recorded_at <= stale_cutoff {
                // Atomically reclaim only if still stale. The conditional UPDATE
                // guards against a TOCTOU race where multiple servers could both
                // see the entry as stale and both claim it.
                conn.execute(
                    "UPDATE seen_webhook_ids SET recorded_at = ?1 \
                     WHERE webhook_id = ?2 AND claim_state = 0 AND recorded_at <= ?3",
                    params![now_secs, webhook_id, stale_cutoff],
                )
                .map_err(|e| e.to_string())?;

                if conn.changes() > 0 {
                    Ok(WebhookClaimResult::Claimed)
                } else {
                    // Another server won the race - return InProgress to trigger retry
                    Ok(WebhookClaimResult::InProgress)
                }
            } else {
                Ok(WebhookClaimResult::InProgress)
            }
        }
        _ => Ok(WebhookClaimResult::Completed),
    }
}

/// Mark a claimed webhook as successfully completed.
pub(super) fn complete_webhook_claim_sync(
    conn: &Connection,
    webhook_id: &str,
) -> Result<(), String> {
    // Update claim_state to 1 (completed)
    conn.execute(
        "UPDATE seen_webhook_ids SET claim_state = 1 WHERE webhook_id = ?1",
        params![webhook_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Release a claimed webhook ID to allow retries.
pub(super) fn release_webhook_claim_sync(
    conn: &Connection,
    webhook_id: &str,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM seen_webhook_ids WHERE webhook_id = ?1",
        params![webhook_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Clean up expired webhook IDs.
pub(super) fn cleanup_expired_webhooks_sync(
    conn: &Connection,
    cutoff: i64,
) -> Result<usize, String> {
    // Only delete completed claims (claim_state=1), not in-progress claims (claim_state=0).
    // A long-running handler could be cleaned up and re-claimed, causing duplicate
    // processing if a retry lands late. In-progress claims will be released by the
    // handler on failure, or eventually expire on their own.
    conn.execute(
        "DELETE FROM seen_webhook_ids WHERE recorded_at <= ?1 AND claim_state = 1",
        params![cutoff],
    )
    .map_err(|e| e.to_string())
}

// =============================================================================
// Async wrappers
// =============================================================================

impl SqliteRepository {
    pub(super) async fn is_webhook_seen_impl(
        &self,
        webhook_id: &str,
    ) -> Result<bool, RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            is_webhook_seen_sync(&conn, &webhook_id)
                .map_err(|e| RepositoryError::storage("is_webhook_seen", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("is_webhook_seen", e.to_string()))?
    }

    pub(super) async fn record_webhook_id_impl(
        &self,
        webhook_id: &str,
    ) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            record_webhook_id_sync(&conn, &webhook_id, now_secs)
                .map_err(|e| RepositoryError::storage("record_webhook_id", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("record_webhook_id", e.to_string()))?
    }

    pub(super) async fn try_claim_webhook_id_impl(
        &self,
        webhook_id: &str,
    ) -> Result<WebhookClaimResult, RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let stale_cutoff = now_secs - STALE_IN_PROGRESS_TTL_SECONDS;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            try_claim_webhook_id_sync(&conn, &webhook_id, now_secs, stale_cutoff)
                .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e.to_string()))?
    }

    pub(super) async fn complete_webhook_claim_impl(
        &self,
        webhook_id: &str,
    ) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            complete_webhook_claim_sync(&conn, &webhook_id)
                .map_err(|e| RepositoryError::storage("complete_webhook_claim", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("complete_webhook_claim", e.to_string()))?
    }

    pub(super) async fn release_webhook_claim_impl(
        &self,
        webhook_id: &str,
    ) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            release_webhook_claim_sync(&conn, &webhook_id)
                .map_err(|e| RepositoryError::storage("release_webhook_claim", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("release_webhook_claim", e.to_string()))?
    }

    pub(super) async fn cleanup_expired_webhooks_impl(
        &self,
        ttl_seconds: i64,
    ) -> Result<usize, RepositoryError> {
        let conn = self.conn.clone();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = now_secs - ttl_seconds;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            cleanup_expired_webhooks_sync(&conn, cutoff)
                .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e))
        })
        .await
        .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e.to_string()))?
    }
}
