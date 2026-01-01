//! SQLite implementation of `StateRepository`.
//!
//! This provides persistent storage that survives service restarts.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};

use super::{RepositoryError, StateRepository, StoredState};
use crate::state_machine::state::ReviewMachineState;
use crate::state_machine::store::StateMachinePrId;

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
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        let conn = Connection::open(path)
            .map_err(|e| RepositoryError::storage("open database", e.to_string()))?;

        // Create schema
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
        .map_err(|e| RepositoryError::storage("create schema", e.to_string()))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "INSERT INTO pr_states (repo_owner, repo_name, pr_number, state_json,
                                        installation_id, has_pending_batch, is_batch_submitting)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(repo_owner, repo_name, pr_number) DO UPDATE SET
                     state_json = excluded.state_json,
                     installation_id = excluded.installation_id,
                     has_pending_batch = excluded.has_pending_batch,
                     is_batch_submitting = excluded.is_batch_submitting",
                params![
                    owner,
                    name,
                    pr_num as i64,
                    state_json,
                    installation_id,
                    has_pending_batch,
                    is_batch_submitting
                ],
            )
            .map_err(|e| RepositoryError::storage("put", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("put", e.to_string()))?
    }

    async fn delete(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        // First get the existing state
        let existing = self.get(id).await?;

        if existing.is_none() {
            return Ok(None);
        }

        let conn = self.conn.clone();
        let owner = id.repo_owner.clone();
        let name = id.repo_name.clone();
        let pr_num = id.pr_number;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "DELETE FROM pr_states
                 WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3",
                params![owner, name, pr_num as i64],
            )
            .map_err(|e| RepositoryError::storage("delete", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("delete", e.to_string()))??;

        Ok(existing)
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
                let (owner, name, pr_num, json, installation_id) =
                    row.map_err(|e| RepositoryError::storage("get_pending", e.to_string()))?;

                let state: ReviewMachineState = serde_json::from_str(&json)
                    .map_err(|_| RepositoryError::corruption("state JSON"))?;

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
                let (owner, name, pr_num, json, installation_id) =
                    row.map_err(|e| RepositoryError::storage("get_submitting", e.to_string()))?;

                let state: ReviewMachineState = serde_json::from_str(&json)
                    .map_err(|_| RepositoryError::corruption("state JSON"))?;

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
    }
}
