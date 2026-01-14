//! Repository abstraction for state machine persistence.
//!
//! This module defines the `StateRepository` trait that abstracts
//! storage operations for PR review states.

mod sqlite;

pub use sqlite::SqliteRepository;

// Re-export dashboard types for convenience
pub use crate::dashboard::types::{DashboardEventType, PrEvent, PrSummary};

/// Result of attempting to claim a webhook ID for processing.
///
/// This three-state result allows callers to distinguish between:
/// - First claim (proceed to process)
/// - Already being processed (return retryable error)
/// - Already completed (return success, skip processing)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookClaimResult {
    /// Successfully claimed the webhook for processing.
    /// The caller should proceed with processing.
    Claimed,
    /// Another request is currently processing this webhook.
    /// The caller should return a retryable error (e.g., 409 Conflict)
    /// so OpenAI will retry later.
    InProgress,
    /// This webhook was already successfully processed.
    /// The caller should return success (200 OK) without reprocessing.
    Completed,
}

use async_trait::async_trait;
use std::fmt;

use super::state::ReviewMachineState;
use super::store::StateMachinePrId;

/// Error type for repository operations.
///
/// This allows callers to distinguish between "not found" (None) and
/// "storage error" (Err), which is critical for crash-recovery.
///
/// # Security
/// Error messages are intentionally kept generic to prevent leaking secrets
/// (like database DSNs or credentials) that might be present in raw backend
/// errors. The `Display` impl only shows the error kind, not raw details.
/// Use `Debug` for troubleshooting in development only.
#[derive(Clone)]
pub enum RepositoryError {
    /// Storage backend is unavailable or failed.
    ///
    /// The optional raw error is stored for debugging but NOT included
    /// in `Display` to prevent accidental secret leakage in logs.
    StorageError {
        /// Brief description of what operation failed (safe for logging)
        operation: &'static str,
        /// Raw error message from backend (NOT included in Display)
        /// This may contain sensitive data like connection strings.
        raw_error: Option<String>,
    },
    /// Data is corrupted or invalid.
    DataCorruption {
        /// Brief description of what was corrupted (safe for logging)
        what: &'static str,
    },
}

impl RepositoryError {
    /// Create a storage error for a specific operation.
    ///
    /// The raw_error is stored for debugging but will not appear in logs.
    pub fn storage(operation: &'static str, raw_error: impl Into<String>) -> Self {
        Self::StorageError {
            operation,
            raw_error: Some(raw_error.into()),
        }
    }

    /// Create a storage error without raw error details.
    pub fn storage_simple(operation: &'static str) -> Self {
        Self::StorageError {
            operation,
            raw_error: None,
        }
    }

    /// Create a data corruption error.
    pub fn corruption(what: &'static str) -> Self {
        Self::DataCorruption { what }
    }
}

impl fmt::Debug for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug shows raw_error only in debug builds to prevent secret leakage in release.
        // In release builds, raw_error is redacted since it may contain DSNs or credentials.
        match self {
            Self::StorageError {
                operation,
                raw_error,
            } => {
                let mut d = f.debug_struct("StorageError");
                d.field("operation", operation);
                #[cfg(debug_assertions)]
                if let Some(raw) = raw_error {
                    d.field("raw_error", raw);
                }
                #[cfg(not(debug_assertions))]
                if raw_error.is_some() {
                    d.field("raw_error", &"[REDACTED]");
                }
                d.finish()
            }
            Self::DataCorruption { what } => f
                .debug_struct("DataCorruption")
                .field("what", what)
                .finish(),
        }
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display intentionally omits raw_error to prevent secret leakage in logs
        match self {
            Self::StorageError { operation, .. } => {
                write!(f, "storage error during {}", operation)
            }
            Self::DataCorruption { what } => {
                write!(f, "data corruption: {}", what)
            }
        }
    }
}

impl std::error::Error for RepositoryError {}

/// Combined state record for persistence.
///
/// This bundles the state machine state with the installation ID,
/// which is needed for GitHub API authentication during batch polling.
#[derive(Debug, Clone)]
pub struct StoredState {
    pub state: ReviewMachineState,
    /// Installation ID for GitHub API authentication.
    /// This is `None` if the state was created before a full event was processed
    /// (e.g., via `get_or_init`). States with `None` installation_id should be
    /// filtered out from batch polling since we can't authenticate to GitHub.
    pub installation_id: Option<u64>,
}

/// Repository trait for persisting PR review states.
///
/// Implementations of this trait provide the actual storage backend.
/// The `StateStore` uses this trait to abstract away storage details,
/// enabling future persistence backends (e.g., SQLite) without changing
/// the state machine coordination logic.
///
/// All methods return `Result` to distinguish between:
/// - Success with data (`Ok(Some(state))` or `Ok(())`)
/// - Key not found (`Ok(None)`)
/// - Storage/IO error (`Err(RepositoryError)`)
///
/// This is critical for crash-recovery: callers can decide whether to
/// use a default value (for "not found") or alert/retry (for errors).
#[async_trait]
pub trait StateRepository: Send + Sync {
    /// Get state for a PR.
    ///
    /// Returns:
    /// - `Ok(Some(state))` if found
    /// - `Ok(None)` if not found
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError>;

    /// Store state for a PR (upsert semantics).
    ///
    /// Returns:
    /// - `Ok(())` on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn put(&self, id: &StateMachinePrId, state: StoredState) -> Result<(), RepositoryError>;

    /// Delete state for a PR.
    ///
    /// Returns:
    /// - `Ok(Some(state))` if state was deleted
    /// - `Ok(None)` if state didn't exist
    /// - `Err(RepositoryError)` if storage operation failed
    async fn delete(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError>;

    /// Get all states with pending batches.
    ///
    /// Returns states where `ReviewMachineState::pending_batch_id()` is `Some`.
    /// This includes:
    /// - `BatchPending` and `AwaitingAncestryCheck` (active batches)
    /// - `Cancelled` with `pending_cancel_batch_id` (cancel-failed batches that may complete)
    ///
    /// Used by the polling loop to discover batches that need status checks.
    ///
    /// Returns:
    /// - `Ok(vec)` with pending states on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get_pending(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError>;

    /// Get all states in BatchSubmitting (for crash recovery).
    ///
    /// Returns states where `ReviewMachineState::is_batch_submitting()` is true.
    /// These represent in-flight batch submissions that may have orphaned batches
    /// at OpenAI if the server crashed during submission.
    ///
    /// Used by reconciliation on startup to find and recover orphaned batches.
    ///
    /// Returns:
    /// - `Ok(vec)` with submitting states on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get_submitting(&self)
        -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError>;

    /// Get all stored states.
    ///
    /// Returns all PR states in the repository, regardless of their state.
    /// Used by the status endpoint to display system state.
    ///
    /// Returns:
    /// - `Ok(vec)` with all states on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get_all(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError>;

    /// Look up a PR by its pending batch ID.
    ///
    /// This is used by the OpenAI webhook handler to find which PR a batch
    /// completion event belongs to. The lookup includes both active batches
    /// (in `BatchPending` and `AwaitingAncestryCheck` states) and batches
    /// that are being cancelled (in `Cancelled` state with `pending_cancel_batch_id`).
    ///
    /// Returns:
    /// - `Ok(Some((id, state)))` if a PR with this batch_id is found
    /// - `Ok(None)` if no PR has this batch_id
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get_by_batch_id(
        &self,
        batch_id: &str,
    ) -> Result<Option<(StateMachinePrId, StoredState)>, RepositoryError>;

    // =========================================================================
    // Webhook replay protection
    // =========================================================================

    /// Check if a webhook ID was successfully processed (completed).
    ///
    /// This checks specifically for *completed* claims, not in-progress ones.
    /// A webhook is considered "seen" only after it has been successfully processed
    /// and `complete_webhook_claim` has been called.
    ///
    /// **Important:** This method is NOT suitable for deduplication at request start.
    /// For idempotent webhook handling, use `try_claim_webhook_id` instead, which
    /// atomically claims the webhook and distinguishes between:
    /// - First claim (proceed to process)
    /// - In-progress (another request is processing - return retryable error)
    /// - Completed (already processed - return success without reprocessing)
    ///
    /// Returns:
    /// - `Ok(true)` if the webhook was successfully completed
    /// - `Ok(false)` if the webhook is unknown, in-progress, or expired
    /// - `Err(RepositoryError)` if storage operation failed
    async fn is_webhook_seen(&self, webhook_id: &str) -> Result<bool, RepositoryError>;

    /// Record a webhook ID to prevent replay attacks.
    ///
    /// The webhook ID should be stored with a timestamp so it can be cleaned
    /// up after the TTL expires (typically matching the webhook timestamp
    /// tolerance window).
    ///
    /// Returns:
    /// - `Ok(())` on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn record_webhook_id(&self, webhook_id: &str) -> Result<(), RepositoryError>;

    /// Atomically try to claim a webhook ID for processing.
    ///
    /// This combines the check and record operations into a single atomic
    /// operation to prevent race conditions where concurrent requests both
    /// pass the "is_webhook_seen" check before either records.
    ///
    /// The claim is initially in "in_progress" state. Call `complete_webhook_claim`
    /// after successful processing to mark it as completed.
    ///
    /// Returns:
    /// - `Ok(Claimed)` if this caller successfully claimed the webhook (first to see it)
    /// - `Ok(InProgress)` if the webhook is currently being processed by another caller
    /// - `Ok(Completed)` if the webhook was already successfully processed
    /// - `Err(RepositoryError)` if storage operation failed
    async fn try_claim_webhook_id(
        &self,
        webhook_id: &str,
    ) -> Result<WebhookClaimResult, RepositoryError>;

    /// Mark a claimed webhook as successfully completed.
    ///
    /// This transitions a webhook from "in_progress" to "completed" state.
    /// Should be called after successful processing to prevent future retries
    /// from reprocessing the same webhook.
    ///
    /// Returns:
    /// - `Ok(())` on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn complete_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError>;

    /// Release a claimed webhook ID to allow retries.
    ///
    /// This should be called when processing fails after successfully claiming
    /// the webhook. Releasing allows OpenAI's retry mechanism to work: the same
    /// webhook ID will be accepted on the next attempt.
    ///
    /// Returns:
    /// - `Ok(())` on success (whether or not the ID existed)
    /// - `Err(RepositoryError)` if storage operation failed
    async fn release_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError>;

    /// Clean up expired webhook IDs.
    ///
    /// This is called periodically or opportunistically to remove webhook IDs
    /// older than the TTL. Implementations may also clean up during other
    /// operations if convenient.
    ///
    /// # Arguments
    /// * `ttl_seconds` - Entries older than this are considered expired
    ///
    /// Returns:
    /// - `Ok(count)` with the number of entries removed
    /// - `Err(RepositoryError)` if storage operation failed
    async fn cleanup_expired_webhooks(&self, ttl_seconds: i64) -> Result<usize, RepositoryError>;
}
