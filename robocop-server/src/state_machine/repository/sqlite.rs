//! SQLite implementation of `StateRepository`.
//!
//! This provides persistent storage that survives service restarts.
//!
//! # Schema Versioning
//!
//! The database has a `schema_version` table that tracks the schema version.
//! When the schema needs to change, increment `CURRENT_SCHEMA_VERSION` and add
//! a migration in `run_migrations()`. Migrations run sequentially from the
//! current version to the target version.
//!
//! # Forward Compatibility
//!
//! When adding new fields to `ReviewMachineState`, use `#[serde(default)]` to
//! ensure old persisted states can still be deserialized. When removing fields
//! or changing types in breaking ways, consider adding a migration that updates
//! or quarantines incompatible rows.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{error, warn};

use super::{RepositoryError, StateRepository, StoredState};
use crate::state_machine::state::ReviewMachineState;
use crate::state_machine::store::StateMachinePrId;

/// Current schema version. Increment this when making schema changes and add
/// corresponding migration logic in `run_migrations()`.
const CURRENT_SCHEMA_VERSION: i64 = 3;

/// SQLite-backed state repository.
///
/// Stores PR states in a SQLite database for persistence across restarts.
/// Uses `tokio::task::spawn_blocking` to run synchronous rusqlite operations
/// without blocking the async runtime.
pub struct SqliteRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRepository {
    /// Create a new SQLite repository at the given path.
    ///
    /// Creates the database file and schema if they don't exist.
    /// Runs any pending migrations if the database exists but has an older schema.
    ///
    /// # Durability
    ///
    /// The database is configured with:
    /// - `journal_mode = WAL` for better concurrency and crash safety
    /// - `synchronous = FULL` for maximum durability (survives OS/power failure)
    /// - `busy_timeout = 5000ms` to handle concurrent access gracefully
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        let path_ref = path.as_ref();

        // Ensure parent directory exists (unless it's :memory: or empty path)
        let path_str = path_ref.to_string_lossy();
        if path_str != ":memory:" && !path_str.is_empty() {
            if let Some(parent) = path_ref.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        RepositoryError::storage(
                            "create database directory",
                            format!("{}: {}", parent.display(), e),
                        )
                    })?;

                    // Set restrictive permissions on state directory (Unix only).
                    // This protects all database files including WAL/SHM files that
                    // SQLite creates with default umask permissions.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let dir_permissions = std::fs::Permissions::from_mode(0o700);
                        if let Err(e) = std::fs::set_permissions(parent, dir_permissions) {
                            warn!(
                                "Failed to set restrictive permissions on state directory: {}",
                                e
                            );
                        }
                    }
                }
            }
        }

        let conn = Connection::open(path_ref)
            .map_err(|e| RepositoryError::storage("open database", e.to_string()))?;

        // Set restrictive permissions on database file (Unix only)
        // The database may contain sensitive state (reconciliation tokens, etc.)
        #[cfg(unix)]
        if path_str != ":memory:" && !path_str.is_empty() {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(path_ref, permissions) {
                warn!(
                    "Failed to set restrictive permissions on database file: {}",
                    e
                );
            }
        }

        // Configure durability settings.
        // We must verify WAL mode was actually enabled - SQLite can silently keep
        // DELETE mode on some filesystems (e.g., network filesystems that don't
        // support shared memory), which would violate our durability/concurrency
        // assumptions.
        //
        // For in-memory databases (:memory:), SQLite returns "memory" as the journal
        // mode, which is expected - there's no durability concern since in-memory
        // databases are ephemeral by design.
        let is_in_memory = path_str == ":memory:";
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| RepositoryError::storage("set journal_mode", e.to_string()))?;

        let journal_mode_ok = journal_mode.eq_ignore_ascii_case("wal")
            || (is_in_memory && journal_mode.eq_ignore_ascii_case("memory"));

        if !journal_mode_ok {
            return Err(RepositoryError::storage(
                "configure journal_mode",
                format!(
                    "Failed to enable WAL mode: SQLite returned '{}' instead of 'wal'. \
                     This can happen on filesystems that don't support shared memory \
                     (e.g., some network filesystems). The database requires WAL mode \
                     for durability and concurrency guarantees.",
                    journal_mode
                ),
            ));
        }

        conn.execute_batch(
            r#"
            PRAGMA synchronous = FULL;
            PRAGMA busy_timeout = 5000;
            "#,
        )
        .map_err(|e| RepositoryError::storage("configure pragmas", e.to_string()))?;

        // Set restrictive permissions on WAL and SHM files (Unix only).
        // SQLite creates these with default umask permissions when WAL mode is enabled.
        // While the directory permissions provide primary protection, we also chmod
        // these files directly as defense in depth.
        #[cfg(unix)]
        if path_str != ":memory:" && !path_str.is_empty() {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);

            let wal_path = format!("{}-wal", path_str);
            if std::path::Path::new(&wal_path).exists() {
                if let Err(e) = std::fs::set_permissions(&wal_path, permissions.clone()) {
                    warn!("Failed to set restrictive permissions on WAL file: {}", e);
                }
            }

            let shm_path = format!("{}-shm", path_str);
            if std::path::Path::new(&shm_path).exists() {
                if let Err(e) = std::fs::set_permissions(&shm_path, permissions) {
                    warn!("Failed to set restrictive permissions on SHM file: {}", e);
                }
            }
        }

        // Create schema version table if it doesn't exist
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_version (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                version INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| RepositoryError::storage("create schema_version table", e.to_string()))?;

        // Get current version (0 if table is empty = fresh database)
        let current_version: i64 = conn
            .query_row(
                "SELECT version FROM schema_version WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| RepositoryError::storage("get schema version", e.to_string()))?
            .unwrap_or(0);

        // Run migrations
        Self::run_migrations(&conn, current_version)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run migrations from `from_version` to `CURRENT_SCHEMA_VERSION`.
    fn run_migrations(conn: &Connection, from_version: i64) -> Result<(), RepositoryError> {
        if from_version > CURRENT_SCHEMA_VERSION {
            return Err(RepositoryError::storage(
                "schema version",
                format!(
                    "Database schema version {} is newer than supported version {}. \
                     Please upgrade the application.",
                    from_version, CURRENT_SCHEMA_VERSION
                ),
            ));
        }

        if from_version == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        // Migration from version 0 (fresh database) to version 1
        if from_version < 1 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS pr_states (
                    repo_owner TEXT NOT NULL,
                    repo_name TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    state_json TEXT NOT NULL,
                    installation_id INTEGER,
                    has_pending_batch INTEGER NOT NULL DEFAULT 0,
                    is_batch_submitting INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (repo_owner, repo_name, pr_number)
                );

                CREATE INDEX IF NOT EXISTS idx_pending
                    ON pr_states(has_pending_batch) WHERE has_pending_batch = 1;
                CREATE INDEX IF NOT EXISTS idx_submitting
                    ON pr_states(is_batch_submitting) WHERE is_batch_submitting = 1;
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v1", e.to_string()))?;
        }

        // Migration from version 1 to version 2: Add batch_id column for webhook lookup
        if from_version < 2 {
            conn.execute_batch(
                r#"
                ALTER TABLE pr_states ADD COLUMN batch_id TEXT;
                CREATE INDEX IF NOT EXISTS idx_batch_id_lookup
                    ON pr_states(batch_id) WHERE batch_id IS NOT NULL;
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v2", e.to_string()))?;
        }

        // Migration from version 2 to version 3: Add seen_webhook_ids table for replay protection
        if from_version < 3 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS seen_webhook_ids (
                    webhook_id TEXT PRIMARY KEY,
                    recorded_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_webhook_recorded_at
                    ON seen_webhook_ids(recorded_at);
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v3", e.to_string()))?;
        }

        // Future migrations would go here:
        // if from_version < 4 { ... }

        // Update schema version
        conn.execute(
            "INSERT OR REPLACE INTO schema_version (id, version) VALUES (1, ?1)",
            params![CURRENT_SCHEMA_VERSION],
        )
        .map_err(|e| RepositoryError::storage("update schema version", e.to_string()))?;

        Ok(())
    }

    /// Create a new in-memory SQLite repository (for testing).
    #[cfg(test)]
    pub fn new_in_memory() -> Result<Self, RepositoryError> {
        Self::new(":memory:")
    }
}

#[async_trait]
impl StateRepository for SqliteRepository {
    async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        let conn = self.conn.clone();
        let owner = id.repo_owner.clone();
        let name = id.repo_name.clone();
        let pr_num = id.pr_number;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let result: Option<(String, Option<u64>)> = conn
                .query_row(
                    "SELECT state_json, installation_id FROM pr_states
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3",
                    params![owner, name, pr_num as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| RepositoryError::storage("get", e.to_string()))?;

            match result {
                Some((json, installation_id)) => {
                    let state: ReviewMachineState = serde_json::from_str(&json)
                        .map_err(|_| RepositoryError::corruption("state JSON"))?;
                    Ok(Some(StoredState {
                        state,
                        installation_id,
                    }))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| RepositoryError::storage("get", e.to_string()))?
    }

    async fn put(&self, id: &StateMachinePrId, state: StoredState) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let owner = id.repo_owner.clone();
        let name = id.repo_name.clone();
        let pr_num = id.pr_number;

        let state_json = serde_json::to_string(&state.state)
            .map_err(|e| RepositoryError::storage("serialize state", e.to_string()))?;

        let has_pending_batch = state.state.pending_batch_id().is_some();
        let is_batch_submitting = state.state.is_batch_submitting();
        let installation_id = state.installation_id;
        // Extract batch_id for indexed lookup (used by webhook handler)
        let batch_id = state.state.pending_batch_id().map(|id| id.0.clone());

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "INSERT INTO pr_states (repo_owner, repo_name, pr_number, state_json,
                                        installation_id, has_pending_batch, is_batch_submitting, batch_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(repo_owner, repo_name, pr_number) DO UPDATE SET
                     state_json = excluded.state_json,
                     installation_id = excluded.installation_id,
                     has_pending_batch = excluded.has_pending_batch,
                     is_batch_submitting = excluded.is_batch_submitting,
                     batch_id = excluded.batch_id",
                params![
                    owner,
                    name,
                    pr_num as i64,
                    state_json,
                    installation_id,
                    has_pending_batch,
                    is_batch_submitting,
                    batch_id
                ],
            )
            .map_err(|e| RepositoryError::storage("put", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("put", e.to_string()))?
    }

    async fn delete(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        let conn = self.conn.clone();
        let owner = id.repo_owner.clone();
        let name = id.repo_name.clone();
        let pr_num = id.pr_number;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Use DELETE...RETURNING to atomically delete and return the row
            let result: Option<(String, Option<u64>)> = conn
                .query_row(
                    "DELETE FROM pr_states
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                     RETURNING state_json, installation_id",
                    params![owner, name, pr_num as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| RepositoryError::storage("delete", e.to_string()))?;

            match result {
                Some((json, installation_id)) => {
                    let state: ReviewMachineState = serde_json::from_str(&json)
                        .map_err(|_| RepositoryError::corruption("state JSON"))?;
                    Ok(Some(StoredState {
                        state,
                        installation_id,
                    }))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| RepositoryError::storage("delete", e.to_string()))?
    }

    async fn get_pending(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn
                .prepare(
                    "SELECT repo_owner, repo_name, pr_number, state_json, installation_id
                     FROM pr_states WHERE has_pending_batch = 1",
                )
                .map_err(|e| RepositoryError::storage("get_pending", e.to_string()))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                })
                .map_err(|e| RepositoryError::storage("get_pending", e.to_string()))?;

            let mut results = Vec::new();
            for row in rows {
                // Skip rows that fail to read from SQLite
                let (owner, name, pr_num, json, installation_id) = match row {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read pending row from SQLite: {}", e);
                        continue;
                    }
                };

                // Skip rows that fail to deserialize - this allows recovery to proceed
                // for other PRs even if one row is corrupt. Log the error for investigation.
                let state: ReviewMachineState = match serde_json::from_str(&json) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "Skipping corrupt state for PR {}/{} #{}: {}. \
                             This row may need manual investigation or will be overwritten \
                             on next state update.",
                            owner, name, pr_num, e
                        );
                        continue;
                    }
                };

                let id = StateMachinePrId::new(owner, name, pr_num as u64);
                results.push((
                    id,
                    StoredState {
                        state,
                        installation_id,
                    },
                ));
            }

            Ok(results)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_pending", e.to_string()))?
    }

    async fn get_submitting(
        &self,
    ) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn
                .prepare(
                    "SELECT repo_owner, repo_name, pr_number, state_json, installation_id
                     FROM pr_states WHERE is_batch_submitting = 1",
                )
                .map_err(|e| RepositoryError::storage("get_submitting", e.to_string()))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                })
                .map_err(|e| RepositoryError::storage("get_submitting", e.to_string()))?;

            let mut results = Vec::new();
            for row in rows {
                // Skip rows that fail to read from SQLite
                let (owner, name, pr_num, json, installation_id) = match row {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read submitting row from SQLite: {}", e);
                        continue;
                    }
                };

                // Skip rows that fail to deserialize - this allows recovery to proceed
                // for other PRs even if one row is corrupt. Log the error for investigation.
                let state: ReviewMachineState = match serde_json::from_str(&json) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "Skipping corrupt state for PR {}/{} #{}: {}. \
                             This row may need manual investigation or will be overwritten \
                             on next state update.",
                            owner, name, pr_num, e
                        );
                        continue;
                    }
                };

                let id = StateMachinePrId::new(owner, name, pr_num as u64);
                results.push((
                    id,
                    StoredState {
                        state,
                        installation_id,
                    },
                ));
            }

            Ok(results)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_submitting", e.to_string()))?
    }

    async fn get_all(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn
                .prepare(
                    "SELECT repo_owner, repo_name, pr_number, state_json, installation_id
                     FROM pr_states ORDER BY repo_owner, repo_name, pr_number",
                )
                .map_err(|e| RepositoryError::storage("get_all", e.to_string()))?;

            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                })
                .map_err(|e| RepositoryError::storage("get_all", e.to_string()))?;

            let mut results = Vec::new();
            for row in rows {
                // Skip rows that fail to read from SQLite
                let (owner, name, pr_num, json, installation_id) = match row {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read row from SQLite: {}", e);
                        continue;
                    }
                };

                // Skip rows that fail to deserialize - this allows the status page to
                // show valid states even if one row is corrupt. Log the error for investigation.
                let state: ReviewMachineState = match serde_json::from_str(&json) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "Skipping corrupt state for PR {}/{} #{}: {}. \
                             This row may need manual investigation or will be overwritten \
                             on next state update.",
                            owner, name, pr_num, e
                        );
                        continue;
                    }
                };

                let id = StateMachinePrId::new(owner, name, pr_num as u64);
                results.push((
                    id,
                    StoredState {
                        state,
                        installation_id,
                    },
                ));
            }

            Ok(results)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_all", e.to_string()))?
    }

    async fn get_by_batch_id(
        &self,
        batch_id: &str,
    ) -> Result<Option<(StateMachinePrId, StoredState)>, RepositoryError> {
        let conn = self.conn.clone();
        let batch_id = batch_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let result: Option<(String, String, i64, String, Option<u64>)> = conn
                .query_row(
                    "SELECT repo_owner, repo_name, pr_number, state_json, installation_id
                     FROM pr_states WHERE batch_id = ?1",
                    params![batch_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| RepositoryError::storage("get_by_batch_id", e.to_string()))?;

            match result {
                Some((owner, name, pr_num, json, installation_id)) => {
                    let state: ReviewMachineState = serde_json::from_str(&json)
                        .map_err(|_| RepositoryError::corruption("state JSON"))?;
                    let id = StateMachinePrId::new(owner, name, pr_num as u64);
                    Ok(Some((
                        id,
                        StoredState {
                            state,
                            installation_id,
                        },
                    )))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| RepositoryError::storage("get_by_batch_id", e.to_string()))?
    }

    // =========================================================================
    // Webhook replay protection
    // =========================================================================

    async fn is_webhook_seen(&self, webhook_id: &str) -> Result<bool, RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM seen_webhook_ids WHERE webhook_id = ?1)",
                    params![webhook_id],
                    |row| row.get(0),
                )
                .map_err(|e| RepositoryError::storage("is_webhook_seen", e.to_string()))?;

            Ok(exists)
        })
        .await
        .map_err(|e| RepositoryError::storage("is_webhook_seen", e.to_string()))?
    }

    async fn record_webhook_id(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "INSERT OR REPLACE INTO seen_webhook_ids (webhook_id, recorded_at) VALUES (?1, ?2)",
                params![webhook_id, now_secs],
            )
            .map_err(|e| RepositoryError::storage("record_webhook_id", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("record_webhook_id", e.to_string()))?
    }

    async fn cleanup_expired_webhooks(&self, ttl_seconds: i64) -> Result<usize, RepositoryError> {
        let conn = self.conn.clone();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = now_secs - ttl_seconds;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let deleted = conn
                .execute(
                    "DELETE FROM seen_webhook_ids WHERE recorded_at <= ?1",
                    params![cutoff],
                )
                .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e.to_string()))?;

            Ok(deleted)
        })
        .await
        .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::{
        BatchId, CancellationReason, CheckRunId, CommentId, CommitSha, FailureReason,
        ReviewMachineState, ReviewOptions, ReviewResult,
    };
    use proptest::prelude::*;

    fn test_pr_id(pr_number: u64) -> StateMachinePrId {
        StateMachinePrId::new("owner", "repo", pr_number)
    }

    fn idle_state(installation_id: u64) -> StoredState {
        StoredState {
            state: ReviewMachineState::Idle {
                reviews_enabled: true,
            },
            installation_id: Some(installation_id),
        }
    }

    fn pending_state(installation_id: u64) -> StoredState {
        StoredState {
            state: ReviewMachineState::BatchPending {
                reviews_enabled: true,
                batch_id: BatchId::from("batch_123".to_string()),
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                comment_id: Some(CommentId(1)),
                check_run_id: Some(CheckRunId(2)),
                model: "gpt-4".to_string(),
                reasoning_effort: "high".to_string(),
            },
            installation_id: Some(installation_id),
        }
    }

    fn submitting_state(installation_id: u64) -> StoredState {
        StoredState {
            state: ReviewMachineState::BatchSubmitting {
                reviews_enabled: true,
                reconciliation_token: "token-123".to_string(),
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                options: ReviewOptions::default(),
                comment_id: None,
                check_run_id: None,
                model: "gpt-4".to_string(),
                reasoning_effort: "high".to_string(),
            },
            installation_id: Some(installation_id),
        }
    }

    #[tokio::test]
    async fn test_get_returns_none_for_missing() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let result = repo.get(&test_pr_id(1)).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_put_then_get() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state.clone()).await.unwrap();
        let result = repo.get(&pr_id).await.unwrap();

        assert!(result.is_some());
        let retrieved = result.unwrap();
        assert_eq!(retrieved.installation_id, Some(12345));
    }

    #[tokio::test]
    async fn test_put_updates_existing() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);

        // Insert initial state
        repo.put(&pr_id, idle_state(111)).await.unwrap();

        // Update to different state
        let updated = StoredState {
            state: ReviewMachineState::Idle {
                reviews_enabled: false,
            },
            installation_id: Some(222),
        };
        repo.put(&pr_id, updated).await.unwrap();

        // Verify update
        let result = repo.get(&pr_id).await.unwrap().unwrap();
        assert_eq!(result.installation_id, Some(222));
        assert!(!result.state.reviews_enabled());
    }

    #[tokio::test]
    async fn test_delete() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state).await.unwrap();
        let deleted = repo.delete(&pr_id).await.unwrap();
        assert!(deleted.is_some());

        let after = repo.get(&pr_id).await.unwrap();
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let deleted = repo.delete(&test_pr_id(999)).await.unwrap();
        assert!(deleted.is_none());
    }

    #[tokio::test]
    async fn test_get_pending_returns_only_pending() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Add an idle state (not pending)
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // Add a pending batch state
        repo.put(&test_pr_id(2), pending_state(222)).await.unwrap();

        // Add another idle state
        repo.put(&test_pr_id(3), idle_state(333)).await.unwrap();

        let pending = repo.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.pr_number, 2);
        assert_eq!(pending[0].1.installation_id, Some(222));
    }

    #[tokio::test]
    async fn test_get_pending_includes_cancelled_with_pending_batch() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Cancelled state WITH pending_cancel_batch_id - must be included
        let cancelled_with_pending = StoredState {
            state: ReviewMachineState::Cancelled {
                reviews_enabled: true,
                head_sha: CommitSha::from("abc123"),
                reason: CancellationReason::Superseded {
                    new_sha: CommitSha::from("def456"),
                },
                pending_cancel_batch_id: Some(BatchId::from("batch_cancel_pending".to_string())),
            },
            installation_id: Some(111),
        };
        repo.put(&test_pr_id(1), cancelled_with_pending)
            .await
            .unwrap();

        // Cancelled state WITHOUT pending_cancel_batch_id - must NOT be included
        let cancelled_without_pending = StoredState {
            state: ReviewMachineState::Cancelled {
                reviews_enabled: true,
                head_sha: CommitSha::from("ghi789"),
                reason: CancellationReason::UserRequested,
                pending_cancel_batch_id: None,
            },
            installation_id: Some(222),
        };
        repo.put(&test_pr_id(2), cancelled_without_pending)
            .await
            .unwrap();

        let pending = repo.get_pending().await.unwrap();

        assert_eq!(
            pending.len(),
            1,
            "Only Cancelled with pending_cancel_batch_id should be returned"
        );
        assert_eq!(pending[0].0.pr_number, 1);
        assert_eq!(pending[0].1.installation_id, Some(111));
    }

    #[tokio::test]
    async fn test_get_submitting_returns_batch_submitting() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Add idle state (not submitting)
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // Add BatchSubmitting state
        repo.put(&test_pr_id(2), submitting_state(222))
            .await
            .unwrap();

        // Add pending state (not submitting)
        repo.put(&test_pr_id(3), pending_state(333)).await.unwrap();

        // get_pending() should NOT return BatchSubmitting states
        let pending = repo.get_pending().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "get_pending() should not return BatchSubmitting states"
        );
        assert_eq!(pending[0].0.pr_number, 3);

        // get_submitting() should return BatchSubmitting states
        let submitting = repo.get_submitting().await.unwrap();
        assert_eq!(
            submitting.len(),
            1,
            "get_submitting() should return BatchSubmitting states for crash recovery"
        );
        assert_eq!(submitting[0].0.pr_number, 2);
    }

    #[tokio::test]
    async fn test_multiple_prs_different_repos() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        let pr1 = StateMachinePrId::new("owner1", "repo1", 1);
        let pr2 = StateMachinePrId::new("owner1", "repo2", 1);
        let pr3 = StateMachinePrId::new("owner2", "repo1", 1);

        repo.put(&pr1, idle_state(111)).await.unwrap();
        repo.put(&pr2, idle_state(222)).await.unwrap();
        repo.put(&pr3, idle_state(333)).await.unwrap();

        // Each should be retrievable independently
        assert_eq!(
            repo.get(&pr1).await.unwrap().unwrap().installation_id,
            Some(111)
        );
        assert_eq!(
            repo.get(&pr2).await.unwrap().unwrap().installation_id,
            Some(222)
        );
        assert_eq!(
            repo.get(&pr3).await.unwrap().unwrap().installation_id,
            Some(333)
        );
    }

    // =========================================================================
    // Property-based tests
    // =========================================================================

    fn arb_commit_sha() -> impl Strategy<Value = CommitSha> {
        "[a-f0-9]{40}".prop_map(CommitSha)
    }

    fn arb_batch_id() -> impl Strategy<Value = BatchId> {
        "batch_[a-zA-Z0-9]{8}".prop_map(BatchId)
    }

    fn arb_cancellation_reason() -> impl Strategy<Value = CancellationReason> {
        prop_oneof![
            Just(CancellationReason::UserRequested),
            arb_commit_sha().prop_map(|sha| CancellationReason::Superseded { new_sha: sha }),
            Just(CancellationReason::ReviewsDisabled),
            Just(CancellationReason::External),
            Just(CancellationReason::NoChanges),
            Just(CancellationReason::DiffTooLarge),
            Just(CancellationReason::NoFiles),
        ]
    }

    fn arb_failure_reason() -> impl Strategy<Value = FailureReason> {
        prop_oneof![
            any::<Option<String>>().prop_map(|error| FailureReason::BatchFailed { error }),
            Just(FailureReason::BatchExpired),
            Just(FailureReason::BatchCancelled),
            any::<String>().prop_map(|error| FailureReason::DownloadFailed { error }),
            any::<String>().prop_map(|error| FailureReason::ParseFailed { error }),
            Just(FailureReason::NoOutputFile),
            any::<String>().prop_map(|error| FailureReason::SubmissionFailed { error }),
            any::<String>().prop_map(|reason| FailureReason::DataFetchFailed { reason }),
        ]
    }

    fn arb_review_result() -> impl Strategy<Value = ReviewResult> {
        (any::<String>(), any::<bool>(), any::<String>()).prop_map(
            |(reasoning, substantive_comments, summary)| ReviewResult {
                reasoning,
                substantive_comments,
                summary,
            },
        )
    }

    fn arb_review_options() -> impl Strategy<Value = ReviewOptions> {
        (any::<Option<String>>(), any::<Option<String>>()).prop_map(|(model, reasoning_effort)| {
            ReviewOptions {
                model,
                reasoning_effort,
            }
        })
    }

    fn arb_review_state() -> impl Strategy<Value = ReviewMachineState> {
        prop_oneof![
            // Idle - no pending batch
            any::<bool>().prop_map(|reviews_enabled| ReviewMachineState::Idle { reviews_enabled }),
            // Preparing - no pending batch
            (
                any::<bool>(),
                arb_commit_sha(),
                arb_commit_sha(),
                arb_review_options()
            )
                .prop_map(|(reviews_enabled, head_sha, base_sha, options)| {
                    ReviewMachineState::Preparing {
                        reviews_enabled,
                        head_sha,
                        base_sha,
                        options,
                    }
                }),
            // BatchSubmitting - is_batch_submitting = true, no pending batch
            (
                any::<bool>(),
                "[a-zA-Z0-9]{16}",
                arb_commit_sha(),
                arb_commit_sha(),
                arb_review_options(),
                any::<Option<u64>>(),
                any::<Option<u64>>(),
                any::<String>(),
                any::<String>(),
            )
                .prop_map(
                    |(
                        reviews_enabled,
                        reconciliation_token,
                        head_sha,
                        base_sha,
                        options,
                        comment_id,
                        check_run_id,
                        model,
                        reasoning_effort,
                    )| {
                        ReviewMachineState::BatchSubmitting {
                            reviews_enabled,
                            reconciliation_token,
                            head_sha,
                            base_sha,
                            options,
                            comment_id: comment_id.map(CommentId),
                            check_run_id: check_run_id.map(CheckRunId),
                            model,
                            reasoning_effort,
                        }
                    }
                ),
            // AwaitingAncestryCheck - HAS pending batch
            (
                any::<bool>(),
                arb_batch_id(),
                arb_commit_sha(),
                arb_commit_sha(),
                any::<Option<u64>>(),
                any::<Option<u64>>(),
                any::<String>(),
                any::<String>(),
                arb_commit_sha(),
                arb_commit_sha(),
                arb_review_options(),
            )
                .prop_map(
                    |(
                        reviews_enabled,
                        batch_id,
                        head_sha,
                        base_sha,
                        comment_id,
                        check_run_id,
                        model,
                        reasoning_effort,
                        new_head_sha,
                        new_base_sha,
                        new_options,
                    )| {
                        ReviewMachineState::AwaitingAncestryCheck {
                            reviews_enabled,
                            batch_id,
                            head_sha,
                            base_sha,
                            comment_id: comment_id.map(CommentId),
                            check_run_id: check_run_id.map(CheckRunId),
                            model,
                            reasoning_effort,
                            new_head_sha,
                            new_base_sha,
                            new_options,
                        }
                    }
                ),
            // BatchPending - HAS pending batch
            (
                any::<bool>(),
                arb_batch_id(),
                arb_commit_sha(),
                arb_commit_sha(),
                any::<Option<u64>>(),
                any::<Option<u64>>(),
                any::<String>(),
                any::<String>(),
            )
                .prop_map(
                    |(
                        reviews_enabled,
                        batch_id,
                        head_sha,
                        base_sha,
                        comment_id,
                        check_run_id,
                        model,
                        reasoning_effort,
                    )| {
                        ReviewMachineState::BatchPending {
                            reviews_enabled,
                            batch_id,
                            head_sha,
                            base_sha,
                            comment_id: comment_id.map(CommentId),
                            check_run_id: check_run_id.map(CheckRunId),
                            model,
                            reasoning_effort,
                        }
                    }
                ),
            // Completed - no pending batch
            (any::<bool>(), arb_commit_sha(), arb_review_result()).prop_map(
                |(reviews_enabled, head_sha, result)| ReviewMachineState::Completed {
                    reviews_enabled,
                    head_sha,
                    result,
                }
            ),
            // Failed - no pending batch
            (any::<bool>(), arb_commit_sha(), arb_failure_reason()).prop_map(
                |(reviews_enabled, head_sha, reason)| ReviewMachineState::Failed {
                    reviews_enabled,
                    head_sha,
                    reason,
                }
            ),
            // Cancelled without pending_cancel_batch_id - no pending batch
            (any::<bool>(), arb_commit_sha(), arb_cancellation_reason()).prop_map(
                |(reviews_enabled, head_sha, reason)| ReviewMachineState::Cancelled {
                    reviews_enabled,
                    head_sha,
                    reason,
                    pending_cancel_batch_id: None,
                }
            ),
            // Cancelled WITH pending_cancel_batch_id - HAS pending batch
            (
                any::<bool>(),
                arb_commit_sha(),
                arb_cancellation_reason(),
                arb_batch_id()
            )
                .prop_map(|(reviews_enabled, head_sha, reason, batch_id)| {
                    ReviewMachineState::Cancelled {
                        reviews_enabled,
                        head_sha,
                        reason,
                        pending_cancel_batch_id: Some(batch_id),
                    }
                }),
        ]
    }

    proptest! {
        /// Property: get_pending() returns exactly those states where pending_batch_id().is_some().
        #[test]
        fn get_pending_matches_pending_batch_id(states in proptest::collection::vec((0u64..1000, arb_review_state(), 1u64..1000000), 0..50)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = SqliteRepository::new_in_memory().unwrap();

                // Insert all states
                for (pr_number, state, installation_id) in &states {
                    let pr_id = test_pr_id(*pr_number);
                    let stored = StoredState {
                        state: state.clone(),
                        installation_id: Some(*installation_id),
                    };
                    repo.put(&pr_id, stored).await.unwrap();
                }

                // Get pending states
                let pending = repo.get_pending().await.unwrap();

                // Property: each returned state must have pending_batch_id().is_some()
                for (pr_id, stored) in &pending {
                    assert!(
                        stored.state.pending_batch_id().is_some(),
                        "get_pending() returned state without pending batch: PR #{}, state {:?}",
                        pr_id.pr_number,
                        stored.state
                    );
                }

                // Property: count matches expected
                let expected_pending: usize = {
                    let mut seen = std::collections::HashMap::new();
                    for (pr_number, state, installation_id) in &states {
                        seen.insert(*pr_number, (state.clone(), *installation_id));
                    }
                    seen.values()
                        .filter(|(state, _)| state.pending_batch_id().is_some())
                        .count()
                };

                assert_eq!(
                    pending.len(),
                    expected_pending,
                    "get_pending() returned {} states but expected {}",
                    pending.len(),
                    expected_pending
                );
            });
        }

        /// Property: get_submitting() returns exactly those states where is_batch_submitting().
        #[test]
        fn get_submitting_matches_is_batch_submitting(states in proptest::collection::vec((0u64..1000, arb_review_state(), 1u64..1000000), 0..50)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = SqliteRepository::new_in_memory().unwrap();

                // Insert all states
                for (pr_number, state, installation_id) in &states {
                    let pr_id = test_pr_id(*pr_number);
                    let stored = StoredState {
                        state: state.clone(),
                        installation_id: Some(*installation_id),
                    };
                    repo.put(&pr_id, stored).await.unwrap();
                }

                // Get submitting states
                let submitting = repo.get_submitting().await.unwrap();

                // Property: each returned state must have is_batch_submitting() == true
                for (pr_id, stored) in &submitting {
                    assert!(
                        stored.state.is_batch_submitting(),
                        "get_submitting() returned non-submitting state: PR #{}, state {:?}",
                        pr_id.pr_number,
                        stored.state
                    );
                }

                // Property: count matches expected
                let expected_submitting: usize = {
                    let mut seen = std::collections::HashMap::new();
                    for (pr_number, state, installation_id) in &states {
                        seen.insert(*pr_number, (state.clone(), *installation_id));
                    }
                    seen.values()
                        .filter(|(state, _)| state.is_batch_submitting())
                        .count()
                };

                assert_eq!(
                    submitting.len(),
                    expected_submitting,
                    "get_submitting() returned {} states but expected {}",
                    submitting.len(),
                    expected_submitting
                );
            });
        }

        /// Property: put then get returns the same state (round-trip).
        #[test]
        fn put_get_roundtrip(state in arb_review_state(), installation_id in proptest::option::of(1u64..1000000)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = SqliteRepository::new_in_memory().unwrap();
                let pr_id = test_pr_id(42);

                let stored = StoredState {
                    state: state.clone(),
                    installation_id,
                };

                repo.put(&pr_id, stored.clone()).await.unwrap();
                let retrieved = repo.get(&pr_id).await.unwrap().unwrap();

                assert_eq!(retrieved.state, state, "State round-trip failed");
                assert_eq!(retrieved.installation_id, installation_id, "installation_id round-trip failed");
            });
        }

        /// Property: on-disk persistence survives close and reopen.
        ///
        /// This is the core durability property: any state written to disk must
        /// be retrievable after closing and reopening the database.
        #[test]
        fn on_disk_persistence_survives_reopen(state in arb_review_state(), installation_id in proptest::option::of(1u64..1000000)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let temp_dir = tempfile::tempdir().unwrap();
                let db_path = temp_dir.path().join("test.db");
                let pr_id = test_pr_id(42);

                let stored = StoredState {
                    state: state.clone(),
                    installation_id,
                };

                // Write state and close repository
                {
                    let repo = SqliteRepository::new(&db_path).unwrap();
                    repo.put(&pr_id, stored.clone()).await.unwrap();
                    // repo is dropped here, simulating a process restart
                }

                // Reopen and verify
                {
                    let repo = SqliteRepository::new(&db_path).unwrap();
                    let retrieved = repo.get(&pr_id).await.unwrap();

                    assert!(
                        retrieved.is_some(),
                        "State was not persisted across database close/reopen.\n\
                         Expected: Some(StoredState {{ state: {:?}, installation_id: {:?} }})\n\
                         Got: None",
                        state, installation_id
                    );

                    let retrieved = retrieved.unwrap();
                    assert_eq!(
                        retrieved.state, state,
                        "State changed after database close/reopen"
                    );
                    assert_eq!(
                        retrieved.installation_id, installation_id,
                        "installation_id changed after database close/reopen"
                    );
                }
            });
        }
    }

    // =========================================================================
    // On-disk persistence tests
    // =========================================================================

    /// Test that states persist across database close and reopen.
    ///
    /// This is the core durability test: write state, close DB, reopen, verify.
    #[tokio::test]
    async fn test_on_disk_persistence_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let pr_id = test_pr_id(1);

        // Write state and close repository
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            repo.put(&pr_id, pending_state(12345)).await.unwrap();
            // repo is dropped here
        }

        // Reopen and verify
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let retrieved = repo.get(&pr_id).await.unwrap();
            assert!(retrieved.is_some(), "State should persist after reopen");
            assert_eq!(retrieved.unwrap().installation_id, Some(12345));
        }
    }

    /// Test that get_pending works correctly after reopen.
    #[tokio::test]
    async fn test_on_disk_get_pending_after_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Write multiple states
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();
            repo.put(&test_pr_id(2), pending_state(222)).await.unwrap();
            repo.put(&test_pr_id(3), pending_state(333)).await.unwrap();
        }

        // Reopen and verify get_pending returns correct states
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let pending = repo.get_pending().await.unwrap();

            assert_eq!(
                pending.len(),
                2,
                "Should have 2 pending states after reopen"
            );

            let pr_numbers: Vec<_> = pending.iter().map(|(id, _)| id.pr_number).collect();
            assert!(pr_numbers.contains(&2));
            assert!(pr_numbers.contains(&3));
        }
    }

    /// Test that get_submitting works correctly after reopen.
    #[tokio::test]
    async fn test_on_disk_get_submitting_after_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Write states including BatchSubmitting
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();
            repo.put(&test_pr_id(2), submitting_state(222))
                .await
                .unwrap();
            repo.put(&test_pr_id(3), pending_state(333)).await.unwrap();
        }

        // Reopen and verify get_submitting returns correct states
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let submitting = repo.get_submitting().await.unwrap();

            assert_eq!(
                submitting.len(),
                1,
                "Should have 1 submitting state after reopen"
            );
            assert_eq!(submitting[0].0.pr_number, 2);
        }
    }

    /// Test that parent directory is created if it doesn't exist.
    #[tokio::test]
    async fn test_creates_parent_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("nested")
            .join("deeply")
            .join("test.db");

        // The parent directory doesn't exist yet
        assert!(!db_path.parent().unwrap().exists());

        let repo = SqliteRepository::new(&db_path).unwrap();
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // Now parent directory should exist
        assert!(db_path.exists());
    }

    /// Test schema version tracking.
    #[tokio::test]
    async fn test_schema_version_persisted() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Create database
        {
            let _repo = SqliteRepository::new(&db_path).unwrap();
        }

        // Verify schema version was written
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let version: i64 = conn
                .query_row(
                    "SELECT version FROM schema_version WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, CURRENT_SCHEMA_VERSION);
        }
    }

    /// Test that corrupt rows are skipped in get_pending.
    #[tokio::test]
    async fn test_corrupt_row_skipped_in_get_pending() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Create database and insert valid state
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            repo.put(&test_pr_id(1), pending_state(111)).await.unwrap();
        }

        // Manually insert a corrupt row directly into SQLite
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO pr_states (repo_owner, repo_name, pr_number, state_json, installation_id, has_pending_batch, is_batch_submitting) \
                 VALUES ('owner', 'repo', 2, 'not valid json', 222, 1, 0)",
                [],
            ).unwrap();
        }

        // get_pending should return the valid row and skip the corrupt one
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let pending = repo.get_pending().await.unwrap();

            assert_eq!(
                pending.len(),
                1,
                "Should skip corrupt row and return only valid state"
            );
            assert_eq!(pending[0].0.pr_number, 1);
        }
    }

    /// Test that corrupt rows are skipped in get_submitting.
    #[tokio::test]
    async fn test_corrupt_row_skipped_in_get_submitting() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Create database and insert valid state
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            repo.put(&test_pr_id(1), submitting_state(111))
                .await
                .unwrap();
        }

        // Manually insert a corrupt row directly into SQLite
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO pr_states (repo_owner, repo_name, pr_number, state_json, installation_id, has_pending_batch, is_batch_submitting) \
                 VALUES ('owner', 'repo', 2, '{\"invalid\": \"state\"}', 222, 0, 1)",
                [],
            ).unwrap();
        }

        // get_submitting should return the valid row and skip the corrupt one
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let submitting = repo.get_submitting().await.unwrap();

            assert_eq!(
                submitting.len(),
                1,
                "Should skip corrupt row and return only valid state"
            );
            assert_eq!(submitting[0].0.pr_number, 1);
        }
    }

    /// Test that WAL mode is enabled for durability.
    #[tokio::test]
    async fn test_wal_mode_enabled() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let _repo = SqliteRepository::new(&db_path).unwrap();

        // Verify WAL mode
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            journal_mode.to_lowercase(),
            "wal",
            "Database should be in WAL mode"
        );
    }

    /// Test that state directory has restrictive permissions (0700).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_state_dir_has_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let state_dir = temp_dir.path().join("state");
        let db_path = state_dir.join("test.db");

        let _repo = SqliteRepository::new(&db_path).unwrap();

        // Verify directory permissions are 0700
        let metadata = std::fs::metadata(&state_dir).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "State directory should have 0700 permissions, got {:o}",
            mode
        );
    }

    /// Test that WAL and SHM files have restrictive permissions (0600).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_wal_shm_have_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Create repository and write something to trigger WAL file creation
        let repo = SqliteRepository::new(&db_path).unwrap();
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // WAL and SHM files should exist after write
        let wal_path = temp_dir.path().join("test.db-wal");
        let shm_path = temp_dir.path().join("test.db-shm");

        // Note: WAL/SHM files may not exist if SQLite hasn't written to them yet,
        // so we only check permissions if they exist
        if wal_path.exists() {
            let mode = std::fs::metadata(&wal_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "WAL file should have 0600 permissions, got {:o}",
                mode
            );
        }

        if shm_path.exists() {
            let mode = std::fs::metadata(&shm_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "SHM file should have 0600 permissions, got {:o}",
                mode
            );
        }
    }
}
