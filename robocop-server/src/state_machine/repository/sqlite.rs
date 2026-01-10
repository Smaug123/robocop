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

use super::{
    DashboardEventType, PrEvent, PrSummary, RepositoryError, StateRepository, StoredState,
    WebhookClaimResult,
};
use crate::state_machine::state::ReviewMachineState;
use crate::state_machine::store::StateMachinePrId;

/// Current schema version. Increment this when making schema changes and add
/// corresponding migration logic in `run_migrations()`.
const CURRENT_SCHEMA_VERSION: i64 = 5;

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
const STALE_IN_PROGRESS_TTL_SECONDS: i64 = 30 * 60;

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

/// Convert a PR number (u64) to i64 for SQLite storage.
///
/// Returns an error if the PR number exceeds i64::MAX, which would
/// cause silent overflow with `as i64`.
fn pr_number_to_i64(pr_number: u64, operation: &'static str) -> Result<i64, RepositoryError> {
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
fn i64_to_pr_number(value: i64, operation: &'static str) -> Result<u64, RepositoryError> {
    u64::try_from(value).map_err(|_| {
        RepositoryError::storage(
            operation,
            format!("invalid negative PR number {} in database", value),
        )
    })
}

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

            // claim_state: 1 = completed
            conn.execute(
                "INSERT OR REPLACE INTO seen_webhook_ids (webhook_id, recorded_at, claim_state) VALUES (?1, ?2, 1)",
                params![webhook_id, now_secs],
            )
            .map_err(|e| RepositoryError::storage("record_webhook_id", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("record_webhook_id", e.to_string()))?
    }

    async fn try_claim_webhook_id(
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

            // Use atomic INSERT OR IGNORE to avoid the read-then-insert race condition.
            // If two processes both see "missing" and try to insert, the loser's insert
            // is silently ignored (no error), and we detect this via changes() == 0.
            conn.execute(
                "INSERT OR IGNORE INTO seen_webhook_ids (webhook_id, recorded_at, claim_state) VALUES (?1, ?2, 0)",
                params![webhook_id, now_secs],
            )
            .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e.to_string()))?;

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
                .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e.to_string()))?;

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
                        .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e.to_string()))?;

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
        })
        .await
        .map_err(|e| RepositoryError::storage("try_claim_webhook_id", e.to_string()))?
    }

    async fn complete_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Update claim_state to 1 (completed)
            conn.execute(
                "UPDATE seen_webhook_ids SET claim_state = 1 WHERE webhook_id = ?1",
                params![webhook_id],
            )
            .map_err(|e| RepositoryError::storage("complete_webhook_claim", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("complete_webhook_claim", e.to_string()))?
    }

    async fn release_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let webhook_id = webhook_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "DELETE FROM seen_webhook_ids WHERE webhook_id = ?1",
                params![webhook_id],
            )
            .map_err(|e| RepositoryError::storage("release_webhook_claim", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("release_webhook_claim", e.to_string()))?
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

            // Only delete completed claims (claim_state=1), not in-progress claims (claim_state=0).
            // A long-running handler could be cleaned up and re-claimed, causing duplicate
            // processing if a retry lands late. In-progress claims will be released by the
            // handler on failure, or eventually expire on their own.
            let deleted = conn
                .execute(
                    "DELETE FROM seen_webhook_ids WHERE recorded_at <= ?1 AND claim_state = 1",
                    params![cutoff],
                )
                .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e.to_string()))?;

            Ok(deleted)
        })
        .await
        .map_err(|e| RepositoryError::storage("cleanup_expired_webhooks", e.to_string()))?
    }

    // =========================================================================
    // Dashboard event logging
    // =========================================================================

    async fn log_event(&self, event: &PrEvent) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let repo_owner = event.repo_owner.clone();
        let repo_name = event.repo_name.clone();
        let pr_number = pr_number_to_i64(event.pr_number, "log_event")?;
        let recorded_at = event.recorded_at;

        // Serialize event_type to JSON for the event_data column
        let event_json = serde_json::to_string(&event.event_type)
            .map_err(|e| RepositoryError::storage("log_event serialize", e.to_string()))?;

        // Extract the variant name for the event_type column (for easier querying)
        let event_type_name = event_type_variant_name(&event.event_type);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "INSERT INTO pr_events (repo_owner, repo_name, pr_number, event_type, event_data, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    repo_owner,
                    repo_name,
                    pr_number,
                    event_type_name,
                    event_json,
                    recorded_at
                ],
            )
            .map_err(|e| RepositoryError::storage("log_event", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("log_event", e.to_string()))?
    }

    async fn get_pr_events(
        &self,
        pr_id: &StateMachinePrId,
        limit: usize,
    ) -> Result<Vec<PrEvent>, RepositoryError> {
        let conn = self.conn.clone();
        let repo_owner = pr_id.repo_owner.clone();
        let repo_name = pr_id.repo_name.clone();
        let pr_number = pr_number_to_i64(pr_id.pr_number, "get_pr_events")?;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn
                .prepare(
                    "SELECT id, repo_owner, repo_name, pr_number, event_data, recorded_at
                     FROM pr_events
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                     ORDER BY recorded_at DESC, id DESC
                     LIMIT ?4",
                )
                .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?;

            let rows = stmt
                .query_map(
                    params![repo_owner, repo_name, pr_number, limit as i64],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?;

            let mut events = Vec::new();
            for row in rows {
                let (id, owner, name, pr_num, event_data, recorded_at) =
                    row.map_err(|e| RepositoryError::storage("get_pr_events row", e.to_string()))?;

                let event_type: DashboardEventType = serde_json::from_str(&event_data)
                    .map_err(|_| RepositoryError::corruption("event_data JSON"))?;

                let pr_number = i64_to_pr_number(pr_num, "get_pr_events")?;

                events.push(PrEvent {
                    id,
                    repo_owner: owner,
                    repo_name: name,
                    pr_number,
                    event_type,
                    recorded_at,
                });
            }

            Ok(events)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?
    }

    async fn get_prs_with_recent_activity(
        &self,
        since_timestamp: i64,
    ) -> Result<Vec<PrSummary>, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Query to get PR summaries with recent events, joined with pr_states for current state.
            // Uses HAVING instead of WHERE so we count ALL events for each PR,
            // while still filtering to only include PRs that have recent activity.
            let mut stmt = conn
                .prepare(
                    "SELECT
                        e.repo_owner,
                        e.repo_name,
                        e.pr_number,
                        COALESCE(s.state_json, '{}') as state_json,
                        MAX(e.recorded_at) as latest_event_at,
                        COUNT(*) as event_count
                     FROM pr_events e
                     LEFT JOIN pr_states s ON
                        e.repo_owner = s.repo_owner AND
                        e.repo_name = s.repo_name AND
                        e.pr_number = s.pr_number
                     GROUP BY e.repo_owner, e.repo_name, e.pr_number
                     HAVING MAX(e.recorded_at) >= ?1
                     ORDER BY latest_event_at DESC",
                )
                .map_err(|e| {
                    RepositoryError::storage("get_prs_with_recent_activity", e.to_string())
                })?;

            let rows = stmt
                .query_map(params![since_timestamp], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|e| {
                    RepositoryError::storage("get_prs_with_recent_activity", e.to_string())
                })?;

            let mut summaries = Vec::new();
            for row in rows {
                let (owner, name, pr_num, state_json, latest_event_at, event_count) =
                    row.map_err(|e| {
                        RepositoryError::storage("get_prs_with_recent_activity row", e.to_string())
                    })?;

                // Safely convert PR number from i64 - skip rows with invalid data
                let pr_number = match i64_to_pr_number(pr_num, "get_prs_with_recent_activity") {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            "Skipping PR with invalid number in {}/{}: {}",
                            owner,
                            name,
                            e
                        );
                        continue;
                    }
                };

                // Parse state to extract current_state name and reviews_enabled
                let (current_state, reviews_enabled) = if state_json == "{}" {
                    ("Unknown".to_string(), true)
                } else {
                    match serde_json::from_str::<ReviewMachineState>(&state_json) {
                        Ok(state) => (state_variant_name(&state), state.reviews_enabled()),
                        Err(_) => ("Unknown".to_string(), true),
                    }
                };

                summaries.push(PrSummary {
                    repo_owner: owner,
                    repo_name: name,
                    pr_number,
                    current_state,
                    latest_event_at,
                    event_count: event_count as usize,
                    reviews_enabled,
                });
            }

            Ok(summaries)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_prs_with_recent_activity", e.to_string()))?
    }

    async fn cleanup_old_events(&self, older_than: i64) -> Result<usize, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let deleted = conn
                .execute(
                    "DELETE FROM pr_events WHERE recorded_at < ?1",
                    params![older_than],
                )
                .map_err(|e| RepositoryError::storage("cleanup_old_events", e.to_string()))?;

            Ok(deleted)
        })
        .await
        .map_err(|e| RepositoryError::storage("cleanup_old_events", e.to_string()))?
    }
}

/// Extract the variant name from a DashboardEventType for storage.
fn event_type_variant_name(event_type: &DashboardEventType) -> &'static str {
    match event_type {
        DashboardEventType::WebhookReceived { .. } => "WebhookReceived",
        DashboardEventType::CommandReceived { .. } => "CommandReceived",
        DashboardEventType::StateTransition { .. } => "StateTransition",
        DashboardEventType::BatchSubmitted { .. } => "BatchSubmitted",
        DashboardEventType::BatchCompleted { .. } => "BatchCompleted",
        DashboardEventType::BatchFailed { .. } => "BatchFailed",
        DashboardEventType::BatchCancelled { .. } => "BatchCancelled",
        DashboardEventType::CommentPosted { .. } => "CommentPosted",
        DashboardEventType::CheckRunCreated { .. } => "CheckRunCreated",
    }
}

/// Extract the variant name from a ReviewMachineState for display.
fn state_variant_name(state: &ReviewMachineState) -> String {
    match state {
        ReviewMachineState::Idle { .. } => "Idle".to_string(),
        ReviewMachineState::Preparing { .. } => "Preparing".to_string(),
        ReviewMachineState::BatchSubmitting { .. } => "BatchSubmitting".to_string(),
        ReviewMachineState::AwaitingAncestryCheck { .. } => "AwaitingAncestryCheck".to_string(),
        ReviewMachineState::BatchPending { .. } => "BatchPending".to_string(),
        ReviewMachineState::Completed { .. } => "Completed".to_string(),
        ReviewMachineState::Failed { .. } => "Failed".to_string(),
        ReviewMachineState::Cancelled { .. } => "Cancelled".to_string(),
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

    // =========================================================================
    // Webhook claim state tests
    // =========================================================================

    /// Test that try_claim_webhook_id returns Claimed for first claim.
    #[tokio::test]
    async fn test_try_claim_returns_claimed_for_first_attempt() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        let result = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result, WebhookClaimResult::Claimed);
    }

    /// Test that try_claim_webhook_id returns InProgress for second claim
    /// before completion.
    #[tokio::test]
    async fn test_try_claim_returns_in_progress_for_concurrent_claim() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // First claim
        let result1 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result1, WebhookClaimResult::Claimed);

        // Second claim before completion should return InProgress
        let result2 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result2, WebhookClaimResult::InProgress);
    }

    /// Test that try_claim_webhook_id returns Completed after
    /// complete_webhook_claim is called.
    #[tokio::test]
    async fn test_try_claim_returns_completed_after_completion() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Claim
        let result1 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result1, WebhookClaimResult::Claimed);

        // Complete
        repo.complete_webhook_claim("webhook_1").await.unwrap();

        // Next claim should return Completed
        let result2 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result2, WebhookClaimResult::Completed);
    }

    /// Test that release_webhook_claim allows re-claiming.
    #[tokio::test]
    async fn test_release_allows_reclaim() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Claim
        let result1 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result1, WebhookClaimResult::Claimed);

        // Release (simulating processing failure)
        repo.release_webhook_claim("webhook_1").await.unwrap();

        // Should be able to claim again
        let result2 = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result2, WebhookClaimResult::Claimed);
    }

    /// Regression test: The bug where returning 200 for in-progress claims
    /// causes dropped batch completions if the first request later fails.
    ///
    /// Scenario:
    /// 1. Request A claims webhook-123 and starts processing
    /// 2. Request A times out (from OpenAI's perspective)
    /// 3. Request B (retry) arrives with the same webhook-123
    /// 4. OLD BEHAVIOR: Returns 200 - OpenAI thinks it's done
    /// 5. Request A later fails and releases the claim
    /// 6. Result: Batch completion is lost forever (no more retries)
    ///
    /// NEW BEHAVIOR: Request B should see InProgress and get a retryable
    /// error (409), so OpenAI will retry again later.
    #[tokio::test]
    async fn test_in_progress_claim_is_retryable_not_terminal() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Request A claims
        let claim_a = repo.try_claim_webhook_id("webhook_123").await.unwrap();
        assert_eq!(claim_a, WebhookClaimResult::Claimed);

        // Request B (retry) arrives while A is still processing
        // This MUST return InProgress (retryable) NOT Completed (terminal)
        let claim_b = repo.try_claim_webhook_id("webhook_123").await.unwrap();
        assert_eq!(
            claim_b,
            WebhookClaimResult::InProgress,
            "Retry during processing must return InProgress, not Completed"
        );

        // Request A fails and releases
        repo.release_webhook_claim("webhook_123").await.unwrap();

        // Request C (another retry) should now be able to claim
        let claim_c = repo.try_claim_webhook_id("webhook_123").await.unwrap();
        assert_eq!(
            claim_c,
            WebhookClaimResult::Claimed,
            "After release, retry should be able to claim"
        );
    }

    /// Test that claim state persists across database close/reopen.
    #[tokio::test]
    async fn test_claim_state_persists_across_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Claim and complete
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let result = repo
                .try_claim_webhook_id("webhook_persistent")
                .await
                .unwrap();
            assert_eq!(result, WebhookClaimResult::Claimed);
            repo.complete_webhook_claim("webhook_persistent")
                .await
                .unwrap();
        }

        // Reopen and verify Completed state persists
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let result = repo
                .try_claim_webhook_id("webhook_persistent")
                .await
                .unwrap();
            assert_eq!(
                result,
                WebhookClaimResult::Completed,
                "Completed state should persist across database reopen"
            );
        }
    }

    /// Test migration from v3 to v4: existing webhook IDs should default to Completed.
    /// This is the correct behavior because any webhook in the v3 table was already
    /// successfully processed (the old code only recorded on success).
    #[tokio::test]
    async fn test_v3_to_v4_migration_defaults_to_completed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Create a v3 database manually
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();

            // Set up v3 schema
            conn.execute_batch(
                r#"
                CREATE TABLE schema_version (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    version INTEGER NOT NULL
                );
                INSERT INTO schema_version (id, version) VALUES (1, 3);

                CREATE TABLE pr_states (
                    repo_owner TEXT NOT NULL,
                    repo_name TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    state_json TEXT NOT NULL,
                    installation_id INTEGER,
                    has_pending_batch INTEGER NOT NULL DEFAULT 0,
                    is_batch_submitting INTEGER NOT NULL DEFAULT 0,
                    batch_id TEXT,
                    PRIMARY KEY (repo_owner, repo_name, pr_number)
                );

                CREATE TABLE seen_webhook_ids (
                    webhook_id TEXT PRIMARY KEY,
                    recorded_at INTEGER NOT NULL
                );
                INSERT INTO seen_webhook_ids (webhook_id, recorded_at) VALUES ('old_webhook', 1234567890);
                "#,
            )
            .unwrap();
        }

        // Open with SqliteRepository (triggers migration to v4)
        {
            let repo = SqliteRepository::new(&db_path).unwrap();

            // The old webhook should be seen as Completed (default from migration)
            let result = repo.try_claim_webhook_id("old_webhook").await.unwrap();
            assert_eq!(
                result,
                WebhookClaimResult::Completed,
                "Pre-existing webhooks from v3 should default to Completed after migration"
            );
        }
    }

    /// Test that stale InProgress claims can be reclaimed.
    ///
    /// Scenario:
    /// 1. Request A claims webhook and starts processing
    /// 2. Server crashes (claim stays InProgress forever)
    /// 3. Server restarts, OpenAI retries after 30+ minutes
    /// 4. The retry should be able to reclaim the stale claim
    ///
    /// This prevents permanent blocking of webhook IDs due to crashes.
    #[tokio::test]
    async fn test_stale_in_progress_claim_can_be_reclaimed() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Claim the webhook
        let claim_a = repo.try_claim_webhook_id("webhook_stale").await.unwrap();
        assert_eq!(claim_a, WebhookClaimResult::Claimed);

        // Simulate the claim being old (older than STALE_IN_PROGRESS_TTL_SECONDS)
        // by directly manipulating the database
        {
            let conn = repo.conn.lock().unwrap();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Set timestamp to 31 minutes ago (stale threshold is 30 minutes)
            let stale_timestamp = now_secs - (31 * 60);
            conn.execute(
                "UPDATE seen_webhook_ids SET recorded_at = ?1 WHERE webhook_id = ?2",
                params![stale_timestamp, "webhook_stale"],
            )
            .unwrap();
        }

        // A retry should be able to reclaim the stale entry
        let claim_b = repo.try_claim_webhook_id("webhook_stale").await.unwrap();
        assert_eq!(
            claim_b,
            WebhookClaimResult::Claimed,
            "Stale InProgress claims should be reclaimable"
        );
    }

    /// Test that fresh InProgress claims cannot be reclaimed.
    ///
    /// Even if the claim is recent, retries should return InProgress,
    /// not reclaim. Only stale claims (older than 30 minutes) can be reclaimed.
    #[tokio::test]
    async fn test_fresh_in_progress_claim_cannot_be_reclaimed() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Claim the webhook
        let claim_a = repo.try_claim_webhook_id("webhook_fresh").await.unwrap();
        assert_eq!(claim_a, WebhookClaimResult::Claimed);

        // Simulate the claim being recent (within STALE_IN_PROGRESS_TTL_SECONDS)
        // by directly manipulating the database
        {
            let conn = repo.conn.lock().unwrap();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Set timestamp to 5 minutes ago (well within the 30 minute threshold)
            let fresh_timestamp = now_secs - (5 * 60);
            conn.execute(
                "UPDATE seen_webhook_ids SET recorded_at = ?1 WHERE webhook_id = ?2",
                params![fresh_timestamp, "webhook_fresh"],
            )
            .unwrap();
        }

        // A retry should NOT be able to reclaim the fresh entry
        let claim_b = repo.try_claim_webhook_id("webhook_fresh").await.unwrap();
        assert_eq!(
            claim_b,
            WebhookClaimResult::InProgress,
            "Fresh InProgress claims should NOT be reclaimable"
        );
    }

    /// Test that Completed claims are not affected by the stale reclaim logic.
    ///
    /// Even if a Completed claim is very old, it should never be reclaimed.
    /// The stale reclaim logic only applies to InProgress claims.
    #[tokio::test]
    async fn test_completed_claims_are_not_reclaimable() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Claim and complete the webhook
        let claim_a = repo
            .try_claim_webhook_id("webhook_completed")
            .await
            .unwrap();
        assert_eq!(claim_a, WebhookClaimResult::Claimed);
        repo.complete_webhook_claim("webhook_completed")
            .await
            .unwrap();

        // Simulate the claim being very old (older than stale threshold)
        // by directly manipulating the database
        {
            let conn = repo.conn.lock().unwrap();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Set timestamp to 2 hours ago
            let old_timestamp = now_secs - (2 * 60 * 60);
            conn.execute(
                "UPDATE seen_webhook_ids SET recorded_at = ?1 WHERE webhook_id = ?2",
                params![old_timestamp, "webhook_completed"],
            )
            .unwrap();
        }

        // A retry should still see Completed, not reclaim
        let claim_b = repo
            .try_claim_webhook_id("webhook_completed")
            .await
            .unwrap();
        assert_eq!(
            claim_b,
            WebhookClaimResult::Completed,
            "Completed claims should never be reclaimable, even if old"
        );
    }

    /// Test that stale InProgress claims persist and can be reclaimed across database reopen.
    ///
    /// This tests the real-world crash recovery scenario where:
    /// 1. Server crashes while processing a webhook
    /// 2. Server restarts (new database connection)
    /// 3. OpenAI retries after 30+ minutes
    /// 4. The retry should be able to reclaim the stale claim
    #[tokio::test]
    async fn test_stale_claim_reclaim_across_reopen() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        // Claim the webhook in first connection
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let claim = repo.try_claim_webhook_id("webhook_crash").await.unwrap();
            assert_eq!(claim, WebhookClaimResult::Claimed);

            // Make the claim stale by manipulating timestamp directly
            let conn = repo.conn.lock().unwrap();
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let stale_timestamp = now_secs - (31 * 60); // 31 minutes ago
            conn.execute(
                "UPDATE seen_webhook_ids SET recorded_at = ?1 WHERE webhook_id = ?2",
                params![stale_timestamp, "webhook_crash"],
            )
            .unwrap();
        }
        // First connection dropped here (simulating crash)

        // Reopen and verify stale claim can be reclaimed
        {
            let repo = SqliteRepository::new(&db_path).unwrap();
            let claim = repo.try_claim_webhook_id("webhook_crash").await.unwrap();
            assert_eq!(
                claim,
                WebhookClaimResult::Claimed,
                "Stale InProgress claims should be reclaimable after database reopen"
            );
        }
    }

    // =========================================================================
    // Dashboard event logging tests
    // =========================================================================

    fn make_test_event(
        pr_number: u64,
        event_type: DashboardEventType,
        recorded_at: i64,
    ) -> PrEvent {
        PrEvent {
            id: 0, // Will be assigned by DB
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number,
            event_type,
            recorded_at,
        }
    }

    /// Test: log_event followed by get_pr_events returns the event.
    #[tokio::test]
    async fn test_log_event_then_get() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);

        let event = make_test_event(
            1,
            DashboardEventType::StateTransition {
                from_state: "Idle".to_string(),
                to_state: "Preparing".to_string(),
                trigger: "PrUpdated".to_string(),
            },
            1000,
        );

        repo.log_event(&event).await.unwrap();

        let events = repo.get_pr_events(&pr_id, 100).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].pr_number, 1);
        assert_eq!(events[0].recorded_at, 1000);

        match &events[0].event_type {
            DashboardEventType::StateTransition {
                from_state,
                to_state,
                trigger,
            } => {
                assert_eq!(from_state, "Idle");
                assert_eq!(to_state, "Preparing");
                assert_eq!(trigger, "PrUpdated");
            }
            _ => panic!("Expected StateTransition event"),
        }
    }

    /// Test: get_pr_events returns events in reverse chronological order.
    #[tokio::test]
    async fn test_get_pr_events_order() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);

        // Log events with different timestamps
        for (i, ts) in [100, 300, 200].iter().enumerate() {
            let event = make_test_event(
                1,
                DashboardEventType::StateTransition {
                    from_state: format!("State{}", i),
                    to_state: format!("State{}", i + 1),
                    trigger: format!("Event{}", i),
                },
                *ts,
            );
            repo.log_event(&event).await.unwrap();
        }

        let events = repo.get_pr_events(&pr_id, 100).await.unwrap();
        assert_eq!(events.len(), 3);

        // Should be ordered by recorded_at DESC
        assert_eq!(events[0].recorded_at, 300);
        assert_eq!(events[1].recorded_at, 200);
        assert_eq!(events[2].recorded_at, 100);
    }

    /// Test: get_pr_events respects limit.
    #[tokio::test]
    async fn test_get_pr_events_limit() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);

        // Log 10 events
        for i in 0..10 {
            let event = make_test_event(
                1,
                DashboardEventType::StateTransition {
                    from_state: format!("State{}", i),
                    to_state: format!("State{}", i + 1),
                    trigger: format!("Event{}", i),
                },
                i as i64,
            );
            repo.log_event(&event).await.unwrap();
        }

        let events = repo.get_pr_events(&pr_id, 3).await.unwrap();
        assert_eq!(events.len(), 3);

        // Should get the 3 most recent (highest timestamps)
        assert_eq!(events[0].recorded_at, 9);
        assert_eq!(events[1].recorded_at, 8);
        assert_eq!(events[2].recorded_at, 7);
    }

    /// Test: get_pr_events only returns events for the requested PR.
    #[tokio::test]
    async fn test_get_pr_events_filters_by_pr() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Log events for different PRs
        for pr_num in [1, 2, 1, 3, 1] {
            let event = make_test_event(
                pr_num,
                DashboardEventType::WebhookReceived {
                    action: "opened".to_string(),
                    head_sha: format!("sha{}", pr_num),
                },
                pr_num as i64 * 100,
            );
            repo.log_event(&event).await.unwrap();
        }

        let events = repo.get_pr_events(&test_pr_id(1), 100).await.unwrap();
        assert_eq!(events.len(), 3);
        for event in events {
            assert_eq!(event.pr_number, 1);
        }
    }

    /// Test: cleanup_old_events removes only old events.
    #[tokio::test]
    async fn test_cleanup_old_events() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Log events with different timestamps
        let old_event = make_test_event(
            1,
            DashboardEventType::WebhookReceived {
                action: "opened".to_string(),
                head_sha: "old".to_string(),
            },
            100, // Old
        );
        let new_event = make_test_event(
            1,
            DashboardEventType::WebhookReceived {
                action: "opened".to_string(),
                head_sha: "new".to_string(),
            },
            300, // New
        );

        repo.log_event(&old_event).await.unwrap();
        repo.log_event(&new_event).await.unwrap();

        // Cleanup events older than 200
        let deleted = repo.cleanup_old_events(200).await.unwrap();
        assert_eq!(deleted, 1);

        // Only the new event should remain
        let events = repo.get_pr_events(&test_pr_id(1), 100).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].recorded_at, 300);
    }

    /// Test: get_prs_with_recent_activity returns PRs with events after threshold.
    #[tokio::test]
    async fn test_get_prs_with_recent_activity() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // Add a PR state for reference
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();
        repo.put(&test_pr_id(2), pending_state(222)).await.unwrap();

        // Log old events for PR#1
        let old_event = make_test_event(
            1,
            DashboardEventType::WebhookReceived {
                action: "opened".to_string(),
                head_sha: "abc".to_string(),
            },
            100,
        );
        repo.log_event(&old_event).await.unwrap();

        // Log recent events for PR#2
        let recent_event = make_test_event(
            2,
            DashboardEventType::StateTransition {
                from_state: "Idle".to_string(),
                to_state: "BatchPending".to_string(),
                trigger: "PrUpdated".to_string(),
            },
            300,
        );
        repo.log_event(&recent_event).await.unwrap();

        // Get PRs with activity after timestamp 200
        let summaries = repo.get_prs_with_recent_activity(200).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].pr_number, 2);
        assert_eq!(summaries[0].current_state, "BatchPending");
        assert_eq!(summaries[0].latest_event_at, 300);
        assert_eq!(summaries[0].event_count, 1);
    }

    /// Test: get_prs_with_recent_activity counts all events for a PR.
    #[tokio::test]
    async fn test_get_prs_with_recent_activity_event_count() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // Log multiple events for the same PR
        for i in 0..5 {
            let event = make_test_event(
                1,
                DashboardEventType::StateTransition {
                    from_state: format!("State{}", i),
                    to_state: format!("State{}", i + 1),
                    trigger: "Event".to_string(),
                },
                100 + i as i64,
            );
            repo.log_event(&event).await.unwrap();
        }

        let summaries = repo.get_prs_with_recent_activity(0).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].event_count, 5);
        assert_eq!(summaries[0].latest_event_at, 104); // Most recent
    }

    /// Test: event_count is total events, not just recent events.
    ///
    /// When filtering by recency, we should still count ALL events for a PR,
    /// not just the ones after the threshold. The threshold determines which
    /// PRs to include (those with any recent activity), but event_count should
    /// reflect the total history.
    #[tokio::test]
    async fn test_event_count_is_total_not_filtered() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        repo.put(&test_pr_id(1), idle_state(111)).await.unwrap();

        // Log 5 events: 3 old (100, 101, 102) and 2 recent (200, 201)
        let timestamps = [100, 101, 102, 200, 201];
        for (i, ts) in timestamps.iter().enumerate() {
            let event = make_test_event(
                1,
                DashboardEventType::StateTransition {
                    from_state: format!("State{}", i),
                    to_state: format!("State{}", i + 1),
                    trigger: "Event".to_string(),
                },
                *ts,
            );
            repo.log_event(&event).await.unwrap();
        }

        // Query with threshold 150 - should include the PR (has events at 200, 201)
        // but event_count should be 5 (total), not 2 (recent only)
        let summaries = repo.get_prs_with_recent_activity(150).await.unwrap();
        assert_eq!(
            summaries.len(),
            1,
            "PR should be included due to recent activity"
        );
        assert_eq!(
            summaries[0].event_count, 5,
            "event_count should be total events (5), not just recent events (2)"
        );
        assert_eq!(summaries[0].latest_event_at, 201);
    }

    /// Test: all DashboardEventType variants serialize and deserialize correctly.
    #[tokio::test]
    async fn test_all_event_types_roundtrip() {
        let repo = SqliteRepository::new_in_memory().unwrap();
        let pr_id = test_pr_id(1);

        let event_types = vec![
            DashboardEventType::WebhookReceived {
                action: "opened".to_string(),
                head_sha: "abc123".to_string(),
            },
            DashboardEventType::CommandReceived {
                command: "review".to_string(),
                user: "testuser".to_string(),
            },
            DashboardEventType::StateTransition {
                from_state: "Idle".to_string(),
                to_state: "Preparing".to_string(),
                trigger: "PrUpdated".to_string(),
            },
            DashboardEventType::BatchSubmitted {
                batch_id: "batch_123".to_string(),
                model: "gpt-4o".to_string(),
            },
            DashboardEventType::BatchCompleted {
                batch_id: "batch_123".to_string(),
                has_issues: true,
            },
            DashboardEventType::BatchFailed {
                batch_id: "batch_123".to_string(),
                reason: "rate limited".to_string(),
            },
            DashboardEventType::BatchCancelled {
                batch_id: Some("batch_123".to_string()),
                reason: "superseded".to_string(),
            },
            DashboardEventType::BatchCancelled {
                batch_id: None,
                reason: "user requested".to_string(),
            },
            DashboardEventType::CommentPosted { comment_id: 12345 },
            DashboardEventType::CheckRunCreated {
                check_run_id: 67890,
            },
        ];

        // Log all event types
        for (i, event_type) in event_types.iter().enumerate() {
            let event = make_test_event(1, event_type.clone(), i as i64);
            repo.log_event(&event).await.unwrap();
        }

        // Retrieve and verify all events roundtrip correctly
        let events = repo.get_pr_events(&pr_id, 100).await.unwrap();
        assert_eq!(events.len(), event_types.len());

        // Events are returned in reverse order
        for (i, event) in events.iter().rev().enumerate() {
            assert_eq!(
                &event.event_type, &event_types[i],
                "Event type mismatch at index {}: expected {:?}, got {:?}",
                i, event_types[i], event.event_type
            );
        }
    }

    /// Test: PR numbers exceeding i64::MAX should error, not silently wrap.
    ///
    /// SQLite stores integers as i64. A u64 PR number larger than i64::MAX
    /// would wrap to a negative value if we use `as i64`. We should reject
    /// such values with an explicit error.
    #[tokio::test]
    async fn test_large_pr_number_returns_error() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // u64::MAX is 18446744073709551615, way larger than i64::MAX (9223372036854775807)
        let large_pr_id = test_pr_id(u64::MAX);

        // Attempting to store should return an error, not silently corrupt
        let result = repo.put(&large_pr_id, idle_state(111)).await;
        assert!(
            result.is_err(),
            "Storing PR number > i64::MAX should error, not silently wrap"
        );
    }

    /// Test: Negative PR numbers in the database should error on read.
    ///
    /// If the database contains a negative pr_number (due to corruption or
    /// past bugs), reading it with `as u64` would silently wrap to a large
    /// positive number. We should detect and reject such values.
    #[tokio::test]
    async fn test_negative_pr_number_in_db_returns_error() {
        let repo = SqliteRepository::new_in_memory().unwrap();

        // First insert a valid state
        repo.put(&test_pr_id(42), idle_state(111)).await.unwrap();

        // Manually corrupt the database by setting a negative PR number
        {
            let conn = repo.conn.lock().unwrap();
            conn.execute(
                "UPDATE pr_states SET pr_number = -1 WHERE pr_number = 42",
                [],
            )
            .unwrap();
        }

        // Attempting to read all states should error, not silently wrap
        let result = repo.get_all().await;
        assert!(
            result.is_err(),
            "Reading negative PR number should error, not silently wrap to large u64"
        );
    }
}
