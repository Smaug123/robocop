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
use rusqlite::Connection;

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

        // Future migrations go here:
        // if from_version < 2 {
        //     Self::migrate_v1_to_v2(conn)?;
        // }

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
}
