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

mod events;
mod webhook;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{error, warn};

use super::{
    PrEvent, PrSummary, RepositoryError, StateRepository, StoredState, WebhookClaimResult,
};
use crate::state_machine::state::ReviewMachineState;
use crate::state_machine::store::StateMachinePrId;

/// Current schema version. Increment this when making schema changes and add
/// corresponding migration logic in `run_migrations()`.
const CURRENT_SCHEMA_VERSION: i64 = 6;

/// SQLite-backed state repository.
///
/// Stores PR states in a SQLite database for persistence across restarts.
/// Uses `tokio::task::spawn_blocking` to run synchronous rusqlite operations
/// without blocking the async runtime.
pub struct SqliteRepository {
    /// Database connection. Exposed as `pub(crate)` for test access to
    /// manipulate timestamps when testing expiry behavior.
    pub(crate) conn: Arc<Mutex<Connection>>,
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

        // Migration from version 3 to version 4: Add claim_state column for tracking
        // in-progress vs completed webhooks. This allows returning 409 for in-progress
        // claims so OpenAI can retry, preventing dropped batch completions.
        // Values: 0 = in_progress, 1 = completed
        if from_version < 4 {
            conn.execute_batch(
                r#"
                ALTER TABLE seen_webhook_ids ADD COLUMN claim_state INTEGER NOT NULL DEFAULT 1;
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v4", e.to_string()))?;
        }

        // Migration from version 4 to version 5: Add pr_events table for dashboard
        // event logging. This enables per-PR timeline display in the dashboard.
        if from_version < 5 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS pr_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repo_owner TEXT NOT NULL,
                    repo_name TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    event_data TEXT,
                    recorded_at INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_pr_events_lookup
                    ON pr_events(repo_owner, repo_name, pr_number, recorded_at DESC);
                CREATE INDEX IF NOT EXISTS idx_pr_events_recent
                    ON pr_events(recorded_at DESC);
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v5", e.to_string()))?;
        }

        // Migration from version 5 to version 6: Make event_data NOT NULL.
        // The log_event function always writes JSON, so NULL values should never exist.
        // SQLite requires table recreation to add NOT NULL constraint.
        if from_version < 6 {
            conn.execute_batch(
                r#"
                CREATE TABLE pr_events_new (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repo_owner TEXT NOT NULL,
                    repo_name TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    event_data TEXT NOT NULL,
                    recorded_at INTEGER NOT NULL
                );

                INSERT INTO pr_events_new (id, repo_owner, repo_name, pr_number, event_type, event_data, recorded_at)
                SELECT id, repo_owner, repo_name, pr_number, event_type, event_data, recorded_at
                FROM pr_events
                WHERE event_data IS NOT NULL;

                DROP TABLE pr_events;

                ALTER TABLE pr_events_new RENAME TO pr_events;

                CREATE INDEX IF NOT EXISTS idx_pr_events_lookup
                    ON pr_events(repo_owner, repo_name, pr_number, recorded_at DESC);
                CREATE INDEX IF NOT EXISTS idx_pr_events_recent
                    ON pr_events(recorded_at DESC);
                "#,
            )
            .map_err(|e| RepositoryError::storage("migration v6", e.to_string()))?;
        }

        // Update schema version
        conn.execute(
            "INSERT OR REPLACE INTO schema_version (id, version) VALUES (1, ?1)",
            params![CURRENT_SCHEMA_VERSION],
        )
        .map_err(|e| RepositoryError::storage("update schema version", e.to_string()))?;

        Ok(())
    }

    /// Create a new in-memory SQLite repository (for testing).
    pub fn new_in_memory() -> Result<Self, RepositoryError> {
        Self::new(":memory:")
    }
}

// =============================================================================
// Integer conversion helpers
// =============================================================================

/// Convert a PR number (u64) to i64 for SQLite storage.
///
/// Returns an error if the PR number exceeds i64::MAX, which would
/// cause silent overflow with `as i64`.
pub(super) fn pr_number_to_i64(
    pr_number: u64,
    operation: &'static str,
) -> Result<i64, RepositoryError> {
    i64::try_from(pr_number).map_err(|_| {
        RepositoryError::storage(
            operation,
            format!(
                "PR number {} exceeds maximum storable value ({})",
                pr_number,
                i64::MAX
            ),
        )
    })
}

/// Convert an i64 from SQLite to a PR number (u64).
///
/// Returns an error if the value is negative, which would indicate
/// database corruption or a bug in previous code.
pub(super) fn i64_to_pr_number(
    value: i64,
    operation: &'static str,
) -> Result<u64, RepositoryError> {
    u64::try_from(value).map_err(|_| {
        RepositoryError::storage(
            operation,
            format!("invalid negative PR number {} in database", value),
        )
    })
}

/// Convert a usize limit to i64 for SQLite LIMIT clause.
///
/// Returns an error if the value exceeds i64::MAX, which would cause
/// silent overflow/wrap with `as i64`. On 64-bit systems, very large
/// usize values can wrap to negative i64, changing SQLite's LIMIT behavior.
pub(super) fn usize_to_i64_limit(
    limit: usize,
    operation: &'static str,
) -> Result<i64, RepositoryError> {
    i64::try_from(limit).map_err(|_| {
        RepositoryError::storage(
            operation,
            format!(
                "limit {} exceeds maximum storable value ({})",
                limit,
                i64::MAX
            ),
        )
    })
}

// =============================================================================
// StateRepository trait implementation
// =============================================================================

#[async_trait]
impl StateRepository for SqliteRepository {
    async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        let conn = self.conn.clone();
        let owner = id.repo_owner.clone();
        let name = id.repo_name.clone();
        let pr_num = pr_number_to_i64(id.pr_number, "get")?;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let result: Option<(String, Option<u64>)> = conn
                .query_row(
                    "SELECT state_json, installation_id FROM pr_states
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3",
                    params![owner, name, pr_num],
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
        let pr_num = pr_number_to_i64(id.pr_number, "put")?;

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
                    pr_num,
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
        let pr_num = pr_number_to_i64(id.pr_number, "delete")?;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Use DELETE...RETURNING to atomically delete and return the row
            let result: Option<(String, Option<u64>)> = conn
                .query_row(
                    "DELETE FROM pr_states
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                     RETURNING state_json, installation_id",
                    params![owner, name, pr_num],
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

                // Skip rows with invalid (negative) PR numbers - indicates database corruption
                let pr_number = match i64_to_pr_number(pr_num, "get_pending") {
                    Ok(n) => n,
                    Err(e) => {
                        error!(
                            "Skipping corrupt row in get_pending: {}. \
                             This indicates database corruption.",
                            e
                        );
                        continue;
                    }
                };

                let id = StateMachinePrId::new(owner, name, pr_number);
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

                // Skip rows with invalid (negative) PR numbers - indicates database corruption
                let pr_number = match i64_to_pr_number(pr_num, "get_submitting") {
                    Ok(n) => n,
                    Err(e) => {
                        error!(
                            "Skipping corrupt row in get_submitting: {}. \
                             This indicates database corruption.",
                            e
                        );
                        continue;
                    }
                };

                let id = StateMachinePrId::new(owner, name, pr_number);
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

                // Return error for invalid (negative) PR numbers - indicates database corruption
                let pr_number = i64_to_pr_number(pr_num, "get_all")?;

                let id = StateMachinePrId::new(owner, name, pr_number);
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
                    let pr_number = i64_to_pr_number(pr_num, "get_by_batch_id")?;
                    let id = StateMachinePrId::new(owner, name, pr_number);
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
    // Webhook replay protection - delegated to webhook module
    // =========================================================================

    async fn is_webhook_seen(&self, webhook_id: &str) -> Result<bool, RepositoryError> {
        self.is_webhook_seen_impl(webhook_id).await
    }

    async fn record_webhook_id(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        self.record_webhook_id_impl(webhook_id).await
    }

    async fn try_claim_webhook_id(
        &self,
        webhook_id: &str,
    ) -> Result<WebhookClaimResult, RepositoryError> {
        self.try_claim_webhook_id_impl(webhook_id).await
    }

    async fn complete_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        self.complete_webhook_claim_impl(webhook_id).await
    }

    async fn release_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        self.release_webhook_claim_impl(webhook_id).await
    }

    async fn cleanup_expired_webhooks(&self, ttl_seconds: i64) -> Result<usize, RepositoryError> {
        self.cleanup_expired_webhooks_impl(ttl_seconds).await
    }

    // =========================================================================
    // Dashboard event logging - delegated to events module
    // =========================================================================

    async fn log_event(&self, event: &PrEvent) -> Result<(), RepositoryError> {
        self.log_event_impl(event).await
    }

    async fn get_pr_events(
        &self,
        pr_id: &StateMachinePrId,
        limit: usize,
    ) -> Result<Vec<PrEvent>, RepositoryError> {
        self.get_pr_events_impl(pr_id, limit).await
    }

    async fn get_prs_with_recent_activity(
        &self,
        since_timestamp: i64,
    ) -> Result<Vec<PrSummary>, RepositoryError> {
        self.get_prs_with_recent_activity_impl(since_timestamp)
            .await
    }

    async fn cleanup_old_events(&self, older_than: i64) -> Result<usize, RepositoryError> {
        self.cleanup_old_events_impl(older_than).await
    }
}
