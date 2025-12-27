//! SQLite persistence layer for PR state machine states.
//!
//! This module provides durable storage for the state machine, enabling
//! restart safety. States are stored with explicit relational columns
//! rather than JSON blobs for type safety and queryability.
//!
//! # Schema Versioning
//!
//! The database uses SQLite's `user_version` pragma to track schema versions.
//! When the schema changes, increment `SCHEMA_VERSION` and add a migration
//! function in `run_migrations`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::state_machine::state::{
    BatchId, CancellationReason, CheckRunId, CommentId, CommitSha, FailureReason,
    ReviewMachineState, ReviewOptions, ReviewResult,
};
use crate::state_machine::store::StateMachinePrId;

/// Current schema version. Increment when making schema changes.
///
/// When adding a new version:
/// 1. Increment this constant
/// 2. Add a migration function `migrate_v{N}_to_v{N+1}`
/// 3. Call it from `run_migrations`
const SCHEMA_VERSION: i32 = 1;

/// Key for batch submission idempotency.
///
/// Used to prevent duplicate batch submissions after crashes.
/// Includes both head_sha and base_sha because the diff (and thus the review)
/// depends on both - if the target branch moves, base_sha changes and we need
/// a new review even if head_sha is the same.
#[derive(Debug, Clone)]
pub struct BatchSubmissionKey {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub head_sha: String,
    pub base_sha: String,
}

/// Cached batch submission data returned when a batch was already submitted.
///
/// Contains all the data needed to reconstruct the `BatchSubmitted` event
/// after recovering from a crash.
#[derive(Debug, Clone)]
pub struct CachedBatchSubmission {
    pub batch_id: String,
    pub comment_id: Option<u64>,
    pub check_run_id: Option<u64>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// Data to store when confirming a batch submission.
#[derive(Debug, Clone)]
pub struct BatchSubmissionData {
    pub batch_id: String,
    pub comment_id: Option<u64>,
    pub check_run_id: Option<u64>,
    pub model: String,
    pub reasoning_effort: String,
}

/// SQLite database for persisting PR state machine states.
///
/// Uses a `Mutex<Connection>` because `rusqlite::Connection` is not `Sync`
/// (it cannot be shared between threads without synchronization). The Mutex
/// provides the required synchronization. Callers should wrap operations in
/// `tokio::task::spawn_blocking` for async compatibility.
pub struct SqliteDb {
    conn: Mutex<Connection>,
}

impl SqliteDb {
    /// Create a new SQLite database connection.
    ///
    /// Opens or creates the database file at the given path.
    /// Use `:memory:` for an in-memory database (useful for testing).
    pub fn new(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite database at {:?}", path))?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema()?;

        Ok(db)
    }

    /// Create an in-memory database (for testing).
    pub fn new_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().context("Failed to open in-memory SQLite database")?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema()?;

        Ok(db)
    }

    /// Initialize the database schema and run any pending migrations.
    ///
    /// Uses SQLite's `user_version` pragma to track the schema version.
    /// Migrations are run in order from the current version to `SCHEMA_VERSION`.
    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > SCHEMA_VERSION {
            anyhow::bail!(
                "Database schema version {} is newer than supported version {}. \
                 Please upgrade the application.",
                current_version,
                SCHEMA_VERSION
            );
        }

        if current_version < SCHEMA_VERSION {
            Self::run_migrations(&conn, current_version)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        Ok(())
    }

    /// Run migrations from `from_version` up to `SCHEMA_VERSION`.
    fn run_migrations(conn: &Connection, from_version: i32) -> Result<()> {
        // Migration v0 -> v1: Initial schema
        if from_version < 1 {
            Self::migrate_v0_to_v1(conn)?;
        }

        Ok(())
    }

    /// Migration v0 -> v1: Create initial schema.
    fn migrate_v0_to_v1(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS pr_states (
                -- Primary key: composite of repo/PR identity
                repo_owner TEXT NOT NULL,
                repo_name TEXT NOT NULL,
                pr_number INTEGER NOT NULL,

                -- Common fields
                state_type TEXT NOT NULL CHECK(state_type IN (
                    'Idle', 'Preparing', 'AwaitingAncestryCheck',
                    'BatchPending', 'Completed', 'Failed', 'Cancelled'
                )),
                reviews_enabled INTEGER NOT NULL,
                installation_id INTEGER NOT NULL,
                updated_at TEXT NOT NULL,

                -- Commit SHAs (used by most states except Idle)
                head_sha TEXT,
                base_sha TEXT,

                -- Batch and GitHub resource IDs
                batch_id TEXT,
                comment_id INTEGER,
                check_run_id INTEGER,
                model TEXT,
                reasoning_effort TEXT,

                -- ReviewOptions for Preparing state
                options_model TEXT,
                options_reasoning_effort TEXT,

                -- AwaitingAncestryCheck-specific fields
                new_head_sha TEXT,
                new_base_sha TEXT,
                new_options_model TEXT,
                new_options_reasoning_effort TEXT,

                -- Completed state: ReviewResult
                result_reasoning TEXT,
                result_substantive_comments INTEGER,
                result_summary TEXT,

                -- Failed state: FailureReason
                failure_type TEXT CHECK(failure_type IS NULL OR failure_type IN (
                    'BatchFailed', 'BatchExpired', 'BatchCancelled',
                    'DownloadFailed', 'ParseFailed', 'NoOutputFile',
                    'SubmissionFailed', 'DataFetchFailed'
                )),
                failure_error TEXT,

                -- Cancelled state: CancellationReason
                cancellation_type TEXT CHECK(cancellation_type IS NULL OR cancellation_type IN (
                    'UserRequested', 'Superseded', 'ReviewsDisabled',
                    'External', 'NoChanges', 'DiffTooLarge', 'NoFiles'
                )),
                cancellation_superseded_sha TEXT,
                pending_cancel_batch_id TEXT,

                PRIMARY KEY (repo_owner, repo_name, pr_number)
            );

            CREATE INDEX IF NOT EXISTS idx_pending_batches
            ON pr_states(repo_owner, repo_name, pr_number)
            WHERE state_type IN ('BatchPending', 'AwaitingAncestryCheck');

            -- Batch submission idempotency table.
            -- Tracks batch submissions to prevent duplicates after crashes.
            -- Key includes both head_sha and base_sha because the diff depends on both.
            CREATE TABLE IF NOT EXISTS batch_submissions (
                repo_owner TEXT NOT NULL,
                repo_name TEXT NOT NULL,
                pr_number INTEGER NOT NULL,
                head_sha TEXT NOT NULL,
                base_sha TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('submitting', 'submitted')),
                batch_id TEXT,  -- NULL while submitting, set after OpenAI confirms
                comment_id INTEGER,
                check_run_id INTEGER,
                model TEXT,
                reasoning_effort TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (repo_owner, repo_name, pr_number, head_sha, base_sha)
            );
            "#,
        )
        .context("Failed to create initial schema (v0 -> v1)")?;

        Ok(())
    }

    /// Insert or update a PR state.
    pub fn upsert_state(
        &self,
        pr_id: &StateMachinePrId,
        state: &ReviewMachineState,
        installation_id: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("mutex poisoned");

        // Extract fields from state
        let (state_type, reviews_enabled) = match state {
            ReviewMachineState::Idle { reviews_enabled } => ("Idle", *reviews_enabled),
            ReviewMachineState::Preparing {
                reviews_enabled, ..
            } => ("Preparing", *reviews_enabled),
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled, ..
            } => ("AwaitingAncestryCheck", *reviews_enabled),
            ReviewMachineState::BatchPending {
                reviews_enabled, ..
            } => ("BatchPending", *reviews_enabled),
            ReviewMachineState::Completed {
                reviews_enabled, ..
            } => ("Completed", *reviews_enabled),
            ReviewMachineState::Failed {
                reviews_enabled, ..
            } => ("Failed", *reviews_enabled),
            ReviewMachineState::Cancelled {
                reviews_enabled, ..
            } => ("Cancelled", *reviews_enabled),
        };

        // Extract nullable fields based on state variant
        let head_sha = state.head_sha().map(|s| s.0.as_str());
        let base_sha = state.base_sha().map(|s| s.0.as_str());

        let (batch_id, comment_id, check_run_id, model, reasoning_effort) = match state {
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                ..
            } => (
                Some(batch_id.0.as_str()),
                comment_id.map(|c| c.0),
                check_run_id.map(|c| c.0),
                Some(model.as_str()),
                Some(reasoning_effort.as_str()),
            ),
            ReviewMachineState::BatchPending {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                ..
            } => (
                Some(batch_id.0.as_str()),
                comment_id.map(|c| c.0),
                check_run_id.map(|c| c.0),
                Some(model.as_str()),
                Some(reasoning_effort.as_str()),
            ),
            _ => (None, None, None, None, None),
        };

        let (options_model, options_reasoning_effort) = match state {
            ReviewMachineState::Preparing { options, .. } => (
                options.model.as_deref(),
                options.reasoning_effort.as_deref(),
            ),
            _ => (None, None),
        };

        let (new_head_sha, new_base_sha, new_options_model, new_options_reasoning_effort) =
            match state {
                ReviewMachineState::AwaitingAncestryCheck {
                    new_head_sha,
                    new_base_sha,
                    new_options,
                    ..
                } => (
                    Some(new_head_sha.0.as_str()),
                    Some(new_base_sha.0.as_str()),
                    new_options.model.as_deref(),
                    new_options.reasoning_effort.as_deref(),
                ),
                _ => (None, None, None, None),
            };

        let (result_reasoning, result_substantive_comments, result_summary) = match state {
            ReviewMachineState::Completed { result, .. } => (
                Some(result.reasoning.as_str()),
                Some(result.substantive_comments),
                Some(result.summary.as_str()),
            ),
            _ => (None, None, None),
        };

        let (failure_type, failure_error) = match state {
            ReviewMachineState::Failed { reason, .. } => match reason {
                FailureReason::BatchFailed { error } => {
                    ("BatchFailed", error.as_ref().map(|s| s.as_str()))
                }
                FailureReason::BatchExpired => ("BatchExpired", None),
                FailureReason::BatchCancelled => ("BatchCancelled", None),
                FailureReason::DownloadFailed { error } => ("DownloadFailed", Some(error.as_str())),
                FailureReason::ParseFailed { error } => ("ParseFailed", Some(error.as_str())),
                FailureReason::NoOutputFile => ("NoOutputFile", None),
                FailureReason::SubmissionFailed { error } => {
                    ("SubmissionFailed", Some(error.as_str()))
                }
                FailureReason::DataFetchFailed { reason } => {
                    ("DataFetchFailed", Some(reason.as_str()))
                }
            },
            _ => ("", None),
        };
        let failure_type: Option<&str> = if failure_type.is_empty() {
            None
        } else {
            Some(failure_type)
        };

        let (cancellation_type, cancellation_superseded_sha, pending_cancel_batch_id) = match state
        {
            ReviewMachineState::Cancelled {
                reason,
                pending_cancel_batch_id,
                ..
            } => {
                let (ctype, superseded) = match reason {
                    CancellationReason::UserRequested => ("UserRequested", None),
                    CancellationReason::Superseded { new_sha } => {
                        ("Superseded", Some(new_sha.0.as_str()))
                    }
                    CancellationReason::ReviewsDisabled => ("ReviewsDisabled", None),
                    CancellationReason::External => ("External", None),
                    CancellationReason::NoChanges => ("NoChanges", None),
                    CancellationReason::DiffTooLarge => ("DiffTooLarge", None),
                    CancellationReason::NoFiles => ("NoFiles", None),
                };
                (
                    Some(ctype),
                    superseded,
                    pending_cancel_batch_id.as_ref().map(|b| b.0.as_str()),
                )
            }
            _ => (None, None, None),
        };

        conn.execute(
            r#"
            INSERT INTO pr_states (
                repo_owner, repo_name, pr_number,
                state_type, reviews_enabled, installation_id, updated_at,
                head_sha, base_sha,
                batch_id, comment_id, check_run_id, model, reasoning_effort,
                options_model, options_reasoning_effort,
                new_head_sha, new_base_sha, new_options_model, new_options_reasoning_effort,
                result_reasoning, result_substantive_comments, result_summary,
                failure_type, failure_error,
                cancellation_type, cancellation_superseded_sha, pending_cancel_batch_id
            )
            VALUES (
                ?1, ?2, ?3,
                ?4, ?5, ?6, datetime('now'),
                ?7, ?8,
                ?9, ?10, ?11, ?12, ?13,
                ?14, ?15,
                ?16, ?17, ?18, ?19,
                ?20, ?21, ?22,
                ?23, ?24,
                ?25, ?26, ?27
            )
            ON CONFLICT (repo_owner, repo_name, pr_number)
            DO UPDATE SET
                state_type = excluded.state_type,
                reviews_enabled = excluded.reviews_enabled,
                installation_id = excluded.installation_id,
                updated_at = excluded.updated_at,
                head_sha = excluded.head_sha,
                base_sha = excluded.base_sha,
                batch_id = excluded.batch_id,
                comment_id = excluded.comment_id,
                check_run_id = excluded.check_run_id,
                model = excluded.model,
                reasoning_effort = excluded.reasoning_effort,
                options_model = excluded.options_model,
                options_reasoning_effort = excluded.options_reasoning_effort,
                new_head_sha = excluded.new_head_sha,
                new_base_sha = excluded.new_base_sha,
                new_options_model = excluded.new_options_model,
                new_options_reasoning_effort = excluded.new_options_reasoning_effort,
                result_reasoning = excluded.result_reasoning,
                result_substantive_comments = excluded.result_substantive_comments,
                result_summary = excluded.result_summary,
                failure_type = excluded.failure_type,
                failure_error = excluded.failure_error,
                cancellation_type = excluded.cancellation_type,
                cancellation_superseded_sha = excluded.cancellation_superseded_sha,
                pending_cancel_batch_id = excluded.pending_cancel_batch_id
            "#,
            rusqlite::params![
                &pr_id.repo_owner,
                &pr_id.repo_name,
                pr_id.pr_number,
                state_type,
                reviews_enabled,
                installation_id,
                head_sha,
                base_sha,
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                options_model,
                options_reasoning_effort,
                new_head_sha,
                new_base_sha,
                new_options_model,
                new_options_reasoning_effort,
                result_reasoning,
                result_substantive_comments,
                result_summary,
                failure_type,
                failure_error,
                cancellation_type,
                cancellation_superseded_sha,
                pending_cancel_batch_id,
            ],
        )
        .context("Failed to upsert PR state")?;

        Ok(())
    }

    /// Delete a PR state.
    pub fn delete_state(&self, pr_id: &StateMachinePrId) -> Result<bool> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let rows_affected = conn
            .execute(
                "DELETE FROM pr_states WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3",
                rusqlite::params![&pr_id.repo_owner, &pr_id.repo_name, pr_id.pr_number,],
            )
            .context("Failed to delete PR state")?;

        Ok(rows_affected > 0)
    }

    /// Load all persisted states from the database.
    ///
    /// Returns a vector of (pr_id, state, installation_id) tuples.
    pub fn load_all_states(&self) -> Result<Vec<(StateMachinePrId, ReviewMachineState, u64)>> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    repo_owner, repo_name, pr_number,
                    state_type, reviews_enabled, installation_id,
                    head_sha, base_sha,
                    batch_id, comment_id, check_run_id, model, reasoning_effort,
                    options_model, options_reasoning_effort,
                    new_head_sha, new_base_sha, new_options_model, new_options_reasoning_effort,
                    result_reasoning, result_substantive_comments, result_summary,
                    failure_type, failure_error,
                    cancellation_type, cancellation_superseded_sha, pending_cancel_batch_id
                FROM pr_states
                "#,
            )
            .context("Failed to prepare load statement")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(StateRow {
                    repo_owner: row.get(0)?,
                    repo_name: row.get(1)?,
                    pr_number: row.get(2)?,
                    state_type: row.get(3)?,
                    reviews_enabled: row.get(4)?,
                    installation_id: row.get(5)?,
                    head_sha: row.get(6)?,
                    base_sha: row.get(7)?,
                    batch_id: row.get(8)?,
                    comment_id: row.get(9)?,
                    check_run_id: row.get(10)?,
                    model: row.get(11)?,
                    reasoning_effort: row.get(12)?,
                    options_model: row.get(13)?,
                    options_reasoning_effort: row.get(14)?,
                    new_head_sha: row.get(15)?,
                    new_base_sha: row.get(16)?,
                    new_options_model: row.get(17)?,
                    new_options_reasoning_effort: row.get(18)?,
                    result_reasoning: row.get(19)?,
                    result_substantive_comments: row.get(20)?,
                    result_summary: row.get(21)?,
                    failure_type: row.get(22)?,
                    failure_error: row.get(23)?,
                    cancellation_type: row.get(24)?,
                    cancellation_superseded_sha: row.get(25)?,
                    pending_cancel_batch_id: row.get(26)?,
                })
            })
            .context("Failed to query PR states")?;

        let mut results = Vec::new();
        for row_result in rows {
            let row = row_result.context("Failed to read row")?;
            let pr_id = StateMachinePrId::new(&row.repo_owner, &row.repo_name, row.pr_number);
            let installation_id = row.installation_id;
            let state = row_to_state(row)?;
            results.push((pr_id, state, installation_id));
        }

        Ok(results)
    }

    /// Get a single PR state from the database.
    pub fn get_state(&self, pr_id: &StateMachinePrId) -> Result<Option<(ReviewMachineState, u64)>> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    repo_owner, repo_name, pr_number,
                    state_type, reviews_enabled, installation_id,
                    head_sha, base_sha,
                    batch_id, comment_id, check_run_id, model, reasoning_effort,
                    options_model, options_reasoning_effort,
                    new_head_sha, new_base_sha, new_options_model, new_options_reasoning_effort,
                    result_reasoning, result_substantive_comments, result_summary,
                    failure_type, failure_error,
                    cancellation_type, cancellation_superseded_sha, pending_cancel_batch_id
                FROM pr_states
                WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                "#,
            )
            .context("Failed to prepare get statement")?;

        let result = stmt.query_row(
            rusqlite::params![&pr_id.repo_owner, &pr_id.repo_name, pr_id.pr_number],
            |row| {
                Ok(StateRow {
                    repo_owner: row.get(0)?,
                    repo_name: row.get(1)?,
                    pr_number: row.get(2)?,
                    state_type: row.get(3)?,
                    reviews_enabled: row.get(4)?,
                    installation_id: row.get(5)?,
                    head_sha: row.get(6)?,
                    base_sha: row.get(7)?,
                    batch_id: row.get(8)?,
                    comment_id: row.get(9)?,
                    check_run_id: row.get(10)?,
                    model: row.get(11)?,
                    reasoning_effort: row.get(12)?,
                    options_model: row.get(13)?,
                    options_reasoning_effort: row.get(14)?,
                    new_head_sha: row.get(15)?,
                    new_base_sha: row.get(16)?,
                    new_options_model: row.get(17)?,
                    new_options_reasoning_effort: row.get(18)?,
                    result_reasoning: row.get(19)?,
                    result_substantive_comments: row.get(20)?,
                    result_summary: row.get(21)?,
                    failure_type: row.get(22)?,
                    failure_error: row.get(23)?,
                    cancellation_type: row.get(24)?,
                    cancellation_superseded_sha: row.get(25)?,
                    pending_cancel_batch_id: row.get(26)?,
                })
            },
        );

        match result {
            Ok(row) => {
                let installation_id = row.installation_id;
                let state = row_to_state(row)?;
                Ok(Some((state, installation_id)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("Failed to get PR state"),
        }
    }

    // =========================================================================
    // Batch submission idempotency methods
    // =========================================================================

    /// Reserve a batch submission slot for idempotency.
    ///
    /// Returns:
    /// - `Ok(None)` if slot was reserved (caller should submit to OpenAI)
    /// - `Ok(Some(CachedBatchSubmission))` if already submitted (caller should skip OpenAI)
    ///
    /// The `requested_model` and `requested_reasoning_effort` parameters are used to
    /// validate that the cached batch (if any) matches the requested options. If the
    /// cached batch used different options, the cache is invalidated and a new
    /// submission is allowed.
    ///
    /// If a row exists with `status = 'submitting'` (prior crash mid-submission),
    /// this returns `Ok(None)` and the caller will re-submit, potentially creating
    /// a duplicate batch in OpenAI. The old batch will expire after 24h.
    ///
    /// # Multi-instance safety
    ///
    /// This method uses `INSERT ... ON CONFLICT DO NOTHING` to avoid races when
    /// multiple server instances share the same database.
    pub fn reserve_batch_submission(
        &self,
        key: &BatchSubmissionKey,
        requested_model: &str,
        requested_reasoning_effort: &str,
    ) -> Result<Option<CachedBatchSubmission>> {
        let conn = self.conn.lock().expect("mutex poisoned");

        // Intermediate struct for query result
        struct BatchRow {
            status: String,
            batch_id: Option<String>,
            comment_id: Option<u64>,
            check_run_id: Option<u64>,
            model: Option<String>,
            reasoning_effort: Option<String>,
        }

        // Helper to fetch existing row
        let fetch_existing = |conn: &Connection| -> Result<Option<BatchRow>> {
            conn.query_row(
                "SELECT status, batch_id, comment_id, check_run_id, model, reasoning_effort \
                 FROM batch_submissions \
                 WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3 AND head_sha = ?4 AND base_sha = ?5",
                rusqlite::params![
                    &key.repo_owner,
                    &key.repo_name,
                    key.pr_number,
                    &key.head_sha,
                    &key.base_sha
                ],
                |row| {
                    Ok(BatchRow {
                        status: row.get(0)?,
                        batch_id: row.get(1)?,
                        comment_id: row.get(2)?,
                        check_run_id: row.get(3)?,
                        model: row.get(4)?,
                        reasoning_effort: row.get(5)?,
                    })
                },
            )
            .optional()
            .context("Failed to query batch_submissions")
        };

        // Helper to delete existing row
        let delete_existing = |conn: &Connection| -> Result<()> {
            conn.execute(
                "DELETE FROM batch_submissions \
                 WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3 AND head_sha = ?4 AND base_sha = ?5",
                rusqlite::params![
                    &key.repo_owner,
                    &key.repo_name,
                    key.pr_number,
                    &key.head_sha,
                    &key.base_sha
                ],
            )
            .context("Failed to delete batch submission")?;
            Ok(())
        };

        // Try to atomically insert a reservation. This handles the race condition:
        // if two instances try to reserve simultaneously, one will succeed and the
        // other will see rows_affected=0 and fetch the existing row.
        let rows_affected = conn
            .execute(
                "INSERT INTO batch_submissions (repo_owner, repo_name, pr_number, head_sha, base_sha, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'submitting') \
                 ON CONFLICT (repo_owner, repo_name, pr_number, head_sha, base_sha) DO NOTHING",
                rusqlite::params![
                    &key.repo_owner,
                    &key.repo_name,
                    key.pr_number,
                    &key.head_sha,
                    &key.base_sha
                ],
            )
            .context("Failed to reserve batch submission")?;

        if rows_affected > 0 {
            // Successfully inserted - no existing row, proceed with submission
            return Ok(None);
        }

        // Row already exists - fetch it to check status and options
        let existing = fetch_existing(&conn)?.ok_or_else(|| {
            anyhow!("Row disappeared after conflict - possible concurrent delete")
        })?;

        match existing.status.as_str() {
            "submitted" => {
                // Check if batch_id is present (should always be for 'submitted')
                let batch_id = match existing.batch_id {
                    Some(bid) => bid,
                    None => {
                        // Corrupted state: submitted without batch_id. Invalidate.
                        delete_existing(&conn)?;
                        conn.execute(
                            "INSERT INTO batch_submissions (repo_owner, repo_name, pr_number, head_sha, base_sha, status) \
                             VALUES (?1, ?2, ?3, ?4, ?5, 'submitting')",
                            rusqlite::params![
                                &key.repo_owner,
                                &key.repo_name,
                                key.pr_number,
                                &key.head_sha,
                                &key.base_sha
                            ],
                        )
                        .context("Failed to reserve batch submission after corruption")?;
                        return Ok(None);
                    }
                };

                // Check if the cached batch used the same options as requested.
                // If options differ, the user is requesting a re-review with different
                // settings, so we should invalidate the cache and allow a new submission.
                let options_match = existing.model.as_deref() == Some(requested_model)
                    && existing.reasoning_effort.as_deref() == Some(requested_reasoning_effort);

                if !options_match {
                    // Options mismatch: invalidate cache and allow new submission.
                    // Note: this creates an orphaned batch in OpenAI (the old one),
                    // but it will expire after 24h.
                    delete_existing(&conn)?;
                    conn.execute(
                        "INSERT INTO batch_submissions (repo_owner, repo_name, pr_number, head_sha, base_sha, status) \
                         VALUES (?1, ?2, ?3, ?4, ?5, 'submitting')",
                        rusqlite::params![
                            &key.repo_owner,
                            &key.repo_name,
                            key.pr_number,
                            &key.head_sha,
                            &key.base_sha
                        ],
                    )
                    .context("Failed to reserve batch submission after options mismatch")?;
                    return Ok(None);
                }

                // Options match - return cached data
                Ok(Some(CachedBatchSubmission {
                    batch_id,
                    comment_id: existing.comment_id,
                    check_run_id: existing.check_run_id,
                    model: existing.model,
                    reasoning_effort: existing.reasoning_effort,
                }))
            }
            "submitting" => {
                // Prior crash mid-submission. Delete stale row and reserve fresh.
                delete_existing(&conn)?;
                conn.execute(
                    "INSERT INTO batch_submissions (repo_owner, repo_name, pr_number, head_sha, base_sha, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'submitting')",
                    rusqlite::params![
                        &key.repo_owner,
                        &key.repo_name,
                        key.pr_number,
                        &key.head_sha,
                        &key.base_sha
                    ],
                )
                .context("Failed to reserve batch submission after stale cleanup")?;
                Ok(None)
            }
            other => {
                // Unknown status - treat as corrupted
                Err(anyhow!("Unknown batch submission status: {}", other))
            }
        }
    }

    /// Confirm a batch submission after OpenAI returns successfully.
    ///
    /// Updates the row from 'submitting' to 'submitted' and records the batch_id
    /// along with UI IDs and options for crash recovery.
    ///
    /// Returns an error if no reservation row exists (e.g., if reserve failed
    /// but the caller proceeded anyway).
    pub fn confirm_batch_submission(
        &self,
        key: &BatchSubmissionKey,
        data: &BatchSubmissionData,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let rows_affected = conn.execute(
            "UPDATE batch_submissions SET \
                status = 'submitted', \
                batch_id = ?6, \
                comment_id = ?7, \
                check_run_id = ?8, \
                model = ?9, \
                reasoning_effort = ?10 \
             WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3 AND head_sha = ?4 AND base_sha = ?5",
            rusqlite::params![
                &key.repo_owner,
                &key.repo_name,
                key.pr_number,
                &key.head_sha,
                &key.base_sha,
                &data.batch_id,
                data.comment_id,
                data.check_run_id,
                &data.model,
                &data.reasoning_effort,
            ],
        )
        .context("Failed to confirm batch submission")?;

        if rows_affected == 0 {
            return Err(anyhow!(
                "No pending submission found to confirm (owner={}, repo={}, pr={}, head={}, base={})",
                key.repo_owner,
                key.repo_name,
                key.pr_number,
                key.head_sha,
                key.base_sha
            ));
        }

        Ok(())
    }

    /// Delete a batch submission reservation (on submission failure).
    pub fn delete_batch_submission(&self, key: &BatchSubmissionKey) -> Result<()> {
        let conn = self.conn.lock().expect("mutex poisoned");

        conn.execute(
            "DELETE FROM batch_submissions \
             WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3 AND head_sha = ?4 AND base_sha = ?5",
            rusqlite::params![
                &key.repo_owner,
                &key.repo_name,
                key.pr_number,
                &key.head_sha,
                &key.base_sha
            ],
        )
        .context("Failed to delete batch submission")?;

        Ok(())
    }

    /// Find all incomplete submissions (status = 'submitting').
    ///
    /// These represent submissions that crashed mid-flight. The caller should
    /// delete these rows so the next event can re-submit.
    pub fn find_incomplete_submissions(&self) -> Result<Vec<BatchSubmissionKey>> {
        let conn = self.conn.lock().expect("mutex poisoned");

        let mut stmt = conn
            .prepare(
                "SELECT repo_owner, repo_name, pr_number, head_sha, base_sha \
                 FROM batch_submissions WHERE status = 'submitting'",
            )
            .context("Failed to prepare find_incomplete_submissions")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(BatchSubmissionKey {
                    repo_owner: row.get(0)?,
                    repo_name: row.get(1)?,
                    pr_number: row.get(2)?,
                    head_sha: row.get(3)?,
                    base_sha: row.get(4)?,
                })
            })
            .context("Failed to query incomplete submissions")?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.context("Failed to read row")?);
        }

        Ok(results)
    }
}

/// Intermediate struct for reading rows from the database.
struct StateRow {
    repo_owner: String,
    repo_name: String,
    pr_number: u64,
    state_type: String,
    reviews_enabled: bool,
    installation_id: u64,
    head_sha: Option<String>,
    base_sha: Option<String>,
    batch_id: Option<String>,
    comment_id: Option<u64>,
    check_run_id: Option<u64>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    options_model: Option<String>,
    options_reasoning_effort: Option<String>,
    new_head_sha: Option<String>,
    new_base_sha: Option<String>,
    new_options_model: Option<String>,
    new_options_reasoning_effort: Option<String>,
    result_reasoning: Option<String>,
    result_substantive_comments: Option<bool>,
    result_summary: Option<String>,
    failure_type: Option<String>,
    failure_error: Option<String>,
    cancellation_type: Option<String>,
    cancellation_superseded_sha: Option<String>,
    pending_cancel_batch_id: Option<String>,
}

/// Convert a database row to a ReviewMachineState.
fn row_to_state(row: StateRow) -> Result<ReviewMachineState> {
    let reviews_enabled = row.reviews_enabled;

    match row.state_type.as_str() {
        "Idle" => Ok(ReviewMachineState::Idle { reviews_enabled }),

        "Preparing" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("Preparing state missing head_sha"))?,
            );
            let base_sha = CommitSha(
                row.base_sha
                    .ok_or_else(|| anyhow!("Preparing state missing base_sha"))?,
            );
            let options = ReviewOptions {
                model: row.options_model,
                reasoning_effort: row.options_reasoning_effort,
            };
            Ok(ReviewMachineState::Preparing {
                reviews_enabled,
                head_sha,
                base_sha,
                options,
            })
        }

        "AwaitingAncestryCheck" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing head_sha"))?,
            );
            let base_sha = CommitSha(
                row.base_sha
                    .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing base_sha"))?,
            );
            let batch_id = BatchId(
                row.batch_id
                    .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing batch_id"))?,
            );
            let model = row
                .model
                .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing model"))?;
            let reasoning_effort = row
                .reasoning_effort
                .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing reasoning_effort"))?;
            let new_head_sha = CommitSha(
                row.new_head_sha
                    .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing new_head_sha"))?,
            );
            let new_base_sha = CommitSha(
                row.new_base_sha
                    .ok_or_else(|| anyhow!("AwaitingAncestryCheck state missing new_base_sha"))?,
            );
            let new_options = ReviewOptions {
                model: row.new_options_model,
                reasoning_effort: row.new_options_reasoning_effort,
            };

            Ok(ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled,
                batch_id,
                head_sha,
                base_sha,
                comment_id: row.comment_id.map(CommentId),
                check_run_id: row.check_run_id.map(CheckRunId),
                model,
                reasoning_effort,
                new_head_sha,
                new_base_sha,
                new_options,
            })
        }

        "BatchPending" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("BatchPending state missing head_sha"))?,
            );
            let base_sha = CommitSha(
                row.base_sha
                    .ok_or_else(|| anyhow!("BatchPending state missing base_sha"))?,
            );
            let batch_id = BatchId(
                row.batch_id
                    .ok_or_else(|| anyhow!("BatchPending state missing batch_id"))?,
            );
            let model = row
                .model
                .ok_or_else(|| anyhow!("BatchPending state missing model"))?;
            let reasoning_effort = row
                .reasoning_effort
                .ok_or_else(|| anyhow!("BatchPending state missing reasoning_effort"))?;

            Ok(ReviewMachineState::BatchPending {
                reviews_enabled,
                batch_id,
                head_sha,
                base_sha,
                comment_id: row.comment_id.map(CommentId),
                check_run_id: row.check_run_id.map(CheckRunId),
                model,
                reasoning_effort,
            })
        }

        "Completed" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("Completed state missing head_sha"))?,
            );
            let result = ReviewResult {
                reasoning: row
                    .result_reasoning
                    .ok_or_else(|| anyhow!("Completed state missing result_reasoning"))?,
                substantive_comments: row.result_substantive_comments.ok_or_else(|| {
                    anyhow!("Completed state missing result_substantive_comments")
                })?,
                summary: row
                    .result_summary
                    .ok_or_else(|| anyhow!("Completed state missing result_summary"))?,
            };

            Ok(ReviewMachineState::Completed {
                reviews_enabled,
                head_sha,
                result,
            })
        }

        "Failed" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("Failed state missing head_sha"))?,
            );
            let failure_type = row
                .failure_type
                .ok_or_else(|| anyhow!("Failed state missing failure_type"))?;
            let reason = match failure_type.as_str() {
                "BatchFailed" => FailureReason::BatchFailed {
                    error: row.failure_error,
                },
                "BatchExpired" => FailureReason::BatchExpired,
                "BatchCancelled" => FailureReason::BatchCancelled,
                "DownloadFailed" => FailureReason::DownloadFailed {
                    error: row
                        .failure_error
                        .ok_or_else(|| anyhow!("DownloadFailed missing error"))?,
                },
                "ParseFailed" => FailureReason::ParseFailed {
                    error: row
                        .failure_error
                        .ok_or_else(|| anyhow!("ParseFailed missing error"))?,
                },
                "NoOutputFile" => FailureReason::NoOutputFile,
                "SubmissionFailed" => FailureReason::SubmissionFailed {
                    error: row
                        .failure_error
                        .ok_or_else(|| anyhow!("SubmissionFailed missing error"))?,
                },
                "DataFetchFailed" => FailureReason::DataFetchFailed {
                    reason: row
                        .failure_error
                        .ok_or_else(|| anyhow!("DataFetchFailed missing reason"))?,
                },
                other => return Err(anyhow!("Unknown failure_type: {}", other)),
            };

            Ok(ReviewMachineState::Failed {
                reviews_enabled,
                head_sha,
                reason,
            })
        }

        "Cancelled" => {
            let head_sha = CommitSha(
                row.head_sha
                    .ok_or_else(|| anyhow!("Cancelled state missing head_sha"))?,
            );
            let cancellation_type = row
                .cancellation_type
                .ok_or_else(|| anyhow!("Cancelled state missing cancellation_type"))?;
            let reason = match cancellation_type.as_str() {
                "UserRequested" => CancellationReason::UserRequested,
                "Superseded" => CancellationReason::Superseded {
                    new_sha: CommitSha(
                        row.cancellation_superseded_sha
                            .ok_or_else(|| anyhow!("Superseded missing new_sha"))?,
                    ),
                },
                "ReviewsDisabled" => CancellationReason::ReviewsDisabled,
                "External" => CancellationReason::External,
                "NoChanges" => CancellationReason::NoChanges,
                "DiffTooLarge" => CancellationReason::DiffTooLarge,
                "NoFiles" => CancellationReason::NoFiles,
                other => return Err(anyhow!("Unknown cancellation_type: {}", other)),
            };

            Ok(ReviewMachineState::Cancelled {
                reviews_enabled,
                head_sha,
                reason,
                pending_cancel_batch_id: row.pending_cancel_batch_id.map(BatchId),
            })
        }

        other => Err(anyhow!("Unknown state_type: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_in_memory() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");
        // Schema should be initialized
        let states = db.load_all_states().expect("should load states");
        assert!(states.is_empty());
    }

    #[test]
    fn test_upsert_and_load_idle() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        let installation_id = 12345u64;

        db.upsert_state(&pr_id, &state, installation_id)
            .expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);

        let (loaded_pr_id, loaded_state, loaded_installation_id) = &states[0];
        assert_eq!(loaded_pr_id, &pr_id);
        assert_eq!(loaded_state, &state);
        assert_eq!(*loaded_installation_id, installation_id);
    }

    #[test]
    fn test_upsert_and_load_batch_pending() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId::from(100)),
            check_run_id: Some(CheckRunId::from(200)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        let installation_id = 12345u64;

        db.upsert_state(&pr_id, &state, installation_id)
            .expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, state);
    }

    #[test]
    fn test_upsert_and_load_completed() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Completed {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            result: ReviewResult {
                reasoning: "This is the reasoning".to_string(),
                substantive_comments: true,
                summary: "Summary here".to_string(),
            },
        };

        db.upsert_state(&pr_id, &state, 100).expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, state);
    }

    #[test]
    fn test_upsert_and_load_failed() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Failed {
            reviews_enabled: false,
            head_sha: CommitSha::from("abc123"),
            reason: FailureReason::BatchFailed {
                error: Some("timeout error".to_string()),
            },
        };

        db.upsert_state(&pr_id, &state, 100).expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, state);
    }

    #[test]
    fn test_upsert_and_load_cancelled() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            reason: CancellationReason::Superseded {
                new_sha: CommitSha::from("new456"),
            },
            pending_cancel_batch_id: Some(BatchId::from("batch_old".to_string())),
        };

        db.upsert_state(&pr_id, &state, 100).expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, state);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state1 = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        let state2 = ReviewMachineState::Idle {
            reviews_enabled: false,
        };

        db.upsert_state(&pr_id, &state1, 100)
            .expect("should upsert");
        db.upsert_state(&pr_id, &state2, 200)
            .expect("should upsert");

        let states = db.load_all_states().expect("should load states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, state2);
        assert_eq!(states[0].2, 200);
    }

    #[test]
    fn test_delete_state() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };

        db.upsert_state(&pr_id, &state, 100).expect("should upsert");

        let deleted = db.delete_state(&pr_id).expect("should delete");
        assert!(deleted);

        let states = db.load_all_states().expect("should load states");
        assert!(states.is_empty());

        let deleted_again = db.delete_state(&pr_id).expect("should delete");
        assert!(!deleted_again);
    }

    #[test]
    fn test_get_state() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };

        let result = db.get_state(&pr_id).expect("should get");
        assert!(result.is_none());

        db.upsert_state(&pr_id, &state, 100).expect("should upsert");

        let result = db.get_state(&pr_id).expect("should get");
        assert!(result.is_some());
        let (loaded_state, loaded_installation_id) = result.unwrap();
        assert_eq!(loaded_state, state);
        assert_eq!(loaded_installation_id, 100);
    }

    #[test]
    fn test_all_state_variants_roundtrip() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let states = [
            ReviewMachineState::Idle {
                reviews_enabled: true,
            },
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                options: ReviewOptions {
                    model: Some("gpt-4".to_string()),
                    reasoning_effort: Some("high".to_string()),
                },
            },
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: true,
                batch_id: BatchId::from("batch_123".to_string()),
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                comment_id: Some(CommentId::from(100)),
                check_run_id: Some(CheckRunId::from(200)),
                model: "gpt-4".to_string(),
                reasoning_effort: "high".to_string(),
                new_head_sha: CommitSha::from("new123"),
                new_base_sha: CommitSha::from("new456"),
                new_options: ReviewOptions::default(),
            },
            ReviewMachineState::BatchPending {
                reviews_enabled: true,
                batch_id: BatchId::from("batch_456".to_string()),
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                comment_id: None,
                check_run_id: None,
                model: "gpt-4".to_string(),
                reasoning_effort: "medium".to_string(),
            },
            ReviewMachineState::Completed {
                reviews_enabled: true,
                head_sha: CommitSha::from("abc123"),
                result: ReviewResult {
                    reasoning: "Analysis complete".to_string(),
                    substantive_comments: true,
                    summary: "Found issues".to_string(),
                },
            },
            ReviewMachineState::Failed {
                reviews_enabled: false,
                head_sha: CommitSha::from("abc123"),
                reason: FailureReason::BatchExpired,
            },
            ReviewMachineState::Cancelled {
                reviews_enabled: true,
                head_sha: CommitSha::from("abc123"),
                reason: CancellationReason::UserRequested,
                pending_cancel_batch_id: None,
            },
        ];

        for (i, state) in states.iter().enumerate() {
            let pr_id = StateMachinePrId::new("owner", "repo", i as u64);
            db.upsert_state(&pr_id, state, 100).expect("should upsert");
        }

        let loaded = db.load_all_states().expect("should load states");
        assert_eq!(loaded.len(), states.len());

        for (i, state) in states.iter().enumerate() {
            let pr_id = StateMachinePrId::new("owner", "repo", i as u64);
            let (loaded_state, _) = db.get_state(&pr_id).expect("should get").unwrap();
            assert_eq!(&loaded_state, state, "State mismatch for variant {}", i);
        }
    }

    #[test]
    fn test_schema_version_is_set() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");
        let conn = db.conn.lock().expect("mutex poisoned");

        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("should query version");

        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_rejects_newer_schema_version() {
        // Create a database with a higher version than we support
        let conn = Connection::open_in_memory().expect("should open");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("should set version");
        drop(conn);

        // We can't easily test this with in-memory since we can't share the connection,
        // so we'll test the error condition directly by creating a file-based db
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_version_{}.db", std::process::id()));

        // Create db with future version
        {
            let conn = Connection::open(&db_path).expect("should open");
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .expect("should set version");
        }

        // Try to open it - should fail
        match SqliteDb::new(&db_path) {
            Ok(_) => panic!("should reject newer schema version"),
            Err(e) => assert!(e.to_string().contains("newer than supported")),
        }

        // Cleanup
        std::fs::remove_file(&db_path).ok();
    }

    #[test]
    fn test_migrations_are_idempotent() {
        // Opening the same database twice should not fail
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_idempotent_{}.db", std::process::id()));

        {
            let _db = SqliteDb::new(&db_path).expect("first open should succeed");
        }

        {
            let _db = SqliteDb::new(&db_path).expect("second open should succeed");
        }

        // Cleanup
        std::fs::remove_file(&db_path).ok();
    }

    // =========================================================================
    // Batch submission idempotency tests
    // =========================================================================

    #[test]
    fn test_reserve_batch_submission_new() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // First reservation should succeed and return None (no existing batch)
        let result = db
            .reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");
        assert!(result.is_none(), "should return None for new reservation");
    }

    #[test]
    fn test_reserve_batch_submission_already_submitted() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve and confirm
        db.reserve_batch_submission(&key, "gpt-4", "high")
            .expect("should reserve");
        let data = BatchSubmissionData {
            batch_id: "batch_123".to_string(),
            comment_id: Some(100),
            check_run_id: Some(200),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        db.confirm_batch_submission(&key, &data)
            .expect("should confirm");

        // Second reservation with same options should return the cached data
        let result = db
            .reserve_batch_submission(&key, "gpt-4", "high")
            .expect("should reserve")
            .expect("should have cached data");
        assert_eq!(
            result.batch_id, "batch_123",
            "should return cached batch_id"
        );
        assert_eq!(result.comment_id, Some(100));
        assert_eq!(result.check_run_id, Some(200));
        assert_eq!(result.model, Some("gpt-4".to_string()));
        assert_eq!(result.reasoning_effort, Some("high".to_string()));
    }

    #[test]
    fn test_reserve_batch_submission_stale_submitting() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve but don't confirm (simulating crash)
        db.reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");

        // Second reservation should delete stale row and reserve fresh
        let result = db
            .reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");
        assert!(
            result.is_none(),
            "should return None after deleting stale row"
        );
    }

    #[test]
    fn test_confirm_batch_submission() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve and confirm
        db.reserve_batch_submission(&key, "gpt-4o", "medium")
            .expect("should reserve");
        let data = BatchSubmissionData {
            batch_id: "batch_456".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4o".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        db.confirm_batch_submission(&key, &data)
            .expect("should confirm");

        // Verify it's now 'submitted'
        let result = db
            .reserve_batch_submission(&key, "gpt-4o", "medium")
            .expect("should reserve")
            .expect("should have cached data");
        assert_eq!(result.batch_id, "batch_456");
    }

    #[test]
    fn test_confirm_without_reserve_fails() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Try to confirm without reserving first
        let data = BatchSubmissionData {
            batch_id: "batch_456".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4o".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        let result = db.confirm_batch_submission(&key, &data);
        assert!(result.is_err(), "confirm without reserve should fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No pending submission"),
            "error should indicate no pending submission"
        );
    }

    #[test]
    fn test_delete_batch_submission() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve
        db.reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");

        // Delete
        db.delete_batch_submission(&key).expect("should delete");

        // Should be able to reserve again
        let result = db
            .reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");
        assert!(result.is_none(), "should return None after delete");
    }

    #[test]
    fn test_find_incomplete_submissions() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key1 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 1,
            head_sha: "sha1".to_string(),
            base_sha: "base1".to_string(),
        };
        let key2 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 2,
            head_sha: "sha2".to_string(),
            base_sha: "base2".to_string(),
        };
        let key3 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 3,
            head_sha: "sha3".to_string(),
            base_sha: "base3".to_string(),
        };

        // key1: submitting (incomplete)
        db.reserve_batch_submission(&key1, "gpt-4", "medium")
            .expect("should reserve");

        // key2: submitted (complete)
        db.reserve_batch_submission(&key2, "gpt-4", "medium")
            .expect("should reserve");
        let data2 = BatchSubmissionData {
            batch_id: "batch_2".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        db.confirm_batch_submission(&key2, &data2)
            .expect("should confirm");

        // key3: submitting (incomplete)
        db.reserve_batch_submission(&key3, "gpt-4", "medium")
            .expect("should reserve");

        // Find incomplete
        let incomplete = db
            .find_incomplete_submissions()
            .expect("should find incomplete");
        assert_eq!(incomplete.len(), 2);

        // Should contain key1 and key3
        let pr_numbers: Vec<u64> = incomplete.iter().map(|k| k.pr_number).collect();
        assert!(pr_numbers.contains(&1));
        assert!(pr_numbers.contains(&3));
        assert!(!pr_numbers.contains(&2));
    }

    #[test]
    fn test_different_head_shas_are_separate() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key1 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "head1".to_string(),
            base_sha: "base".to_string(),
        };
        let key2 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "head2".to_string(),
            base_sha: "base".to_string(),
        };

        // Reserve and confirm key1
        db.reserve_batch_submission(&key1, "gpt-4", "medium")
            .expect("should reserve");
        let data1 = BatchSubmissionData {
            batch_id: "batch_1".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        db.confirm_batch_submission(&key1, &data1)
            .expect("should confirm");

        // key2 should be independent (different head_sha)
        let result = db
            .reserve_batch_submission(&key2, "gpt-4", "medium")
            .expect("should reserve");
        assert!(result.is_none(), "different head_sha should be independent");
    }

    #[test]
    fn test_different_base_shas_are_separate() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key1 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
            base_sha: "base1".to_string(),
        };
        let key2 = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "head".to_string(),
            base_sha: "base2".to_string(),
        };

        // Reserve and confirm key1
        db.reserve_batch_submission(&key1, "gpt-4", "medium")
            .expect("should reserve");
        let data1 = BatchSubmissionData {
            batch_id: "batch_1".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        db.confirm_batch_submission(&key1, &data1)
            .expect("should confirm");

        // key2 should be independent (different base_sha)
        let result = db
            .reserve_batch_submission(&key2, "gpt-4", "medium")
            .expect("should reserve");
        assert!(
            result.is_none(),
            "different base_sha should be independent - this tests Issue 2"
        );
    }

    #[test]
    fn test_options_mismatch_invalidates_cache() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve and confirm with model=gpt-4, reasoning=high
        db.reserve_batch_submission(&key, "gpt-4", "high")
            .expect("should reserve");
        let data = BatchSubmissionData {
            batch_id: "batch_123".to_string(),
            comment_id: Some(100),
            check_run_id: Some(200),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        db.confirm_batch_submission(&key, &data)
            .expect("should confirm");

        // Second reservation with DIFFERENT model should invalidate cache
        let result = db
            .reserve_batch_submission(&key, "gpt-4o", "high")
            .expect("should reserve");
        assert!(
            result.is_none(),
            "different model should invalidate cache and return None"
        );

        // After invalidation, we should be able to confirm again
        let data2 = BatchSubmissionData {
            batch_id: "batch_456".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4o".to_string(),
            reasoning_effort: "high".to_string(),
        };
        db.confirm_batch_submission(&key, &data2)
            .expect("should confirm");

        // Verify the new batch is cached
        let result = db
            .reserve_batch_submission(&key, "gpt-4o", "high")
            .expect("should reserve")
            .expect("should have cached data");
        assert_eq!(result.batch_id, "batch_456");
    }

    #[test]
    fn test_reasoning_effort_mismatch_invalidates_cache() {
        let db = SqliteDb::new_in_memory().expect("should create in-memory db");

        let key = BatchSubmissionKey {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 42,
            head_sha: "abc123".to_string(),
            base_sha: "def456".to_string(),
        };

        // Reserve and confirm with reasoning=medium
        db.reserve_batch_submission(&key, "gpt-4", "medium")
            .expect("should reserve");
        let data = BatchSubmissionData {
            batch_id: "batch_123".to_string(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        db.confirm_batch_submission(&key, &data)
            .expect("should confirm");

        // Second reservation with DIFFERENT reasoning_effort should invalidate cache
        let result = db
            .reserve_batch_submission(&key, "gpt-4", "high")
            .expect("should reserve");
        assert!(
            result.is_none(),
            "different reasoning_effort should invalidate cache and return None"
        );
    }
}
