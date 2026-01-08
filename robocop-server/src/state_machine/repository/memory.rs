//! In-memory implementation of `StateRepository`.
//!
//! This provides the same behavior as the original `StateStore` implementation:
//! all state is held in memory and lost on restart.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::{RepositoryError, StateRepository, StoredState, WebhookClaimResult};
use crate::state_machine::store::StateMachinePrId;

/// State of a webhook claim for idempotent processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebhookClaimState {
    /// Webhook is currently being processed.
    InProgress,
    /// Webhook was successfully processed.
    Completed,
}

/// In-memory state repository.
///
/// Stores PR states in a `HashMap` protected by a `RwLock`.
/// All state is lost on restart.
pub struct InMemoryRepository {
    states: RwLock<HashMap<StateMachinePrId, StoredState>>,
    /// Webhook IDs with their claim state and recording timestamp (unix seconds).
    /// Used for replay attack prevention and idempotent processing.
    webhook_claims: RwLock<HashMap<String, (WebhookClaimState, i64)>>,
}

impl InMemoryRepository {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            webhook_claims: RwLock::new(HashMap::new()),
        }
    }

    /// Get current unix timestamp in seconds.
    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

impl Default for InMemoryRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateRepository for InMemoryRepository {
    async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        let states = self.states.read().await;
        Ok(states.get(id).cloned())
    }

    async fn put(&self, id: &StateMachinePrId, state: StoredState) -> Result<(), RepositoryError> {
        let mut states = self.states.write().await;
        states.insert(id.clone(), state);
        Ok(())
    }

    async fn delete(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
        let mut states = self.states.write().await;
        Ok(states.remove(id))
    }

    async fn get_pending(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let states = self.states.read().await;
        Ok(states
            .iter()
            .filter(|(_, stored)| stored.state.pending_batch_id().is_some())
            .map(|(id, stored)| (id.clone(), stored.clone()))
            .collect())
    }

    async fn get_submitting(
        &self,
    ) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let states = self.states.read().await;
        Ok(states
            .iter()
            .filter(|(_, stored)| stored.state.is_batch_submitting())
            .map(|(id, stored)| (id.clone(), stored.clone()))
            .collect())
    }

    async fn get_all(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
        let states = self.states.read().await;
        Ok(states
            .iter()
            .map(|(id, stored)| (id.clone(), stored.clone()))
            .collect())
    }

    async fn get_by_batch_id(
        &self,
        batch_id: &str,
    ) -> Result<Option<(StateMachinePrId, StoredState)>, RepositoryError> {
        let states = self.states.read().await;
        Ok(states
            .iter()
            .find(|(_, stored)| {
                stored
                    .state
                    .pending_batch_id()
                    .is_some_and(|id| id.0 == batch_id)
            })
            .map(|(id, stored)| (id.clone(), stored.clone())))
    }

    // =========================================================================
    // Webhook replay protection
    // =========================================================================

    async fn is_webhook_seen(&self, webhook_id: &str) -> Result<bool, RepositoryError> {
        let claims = self.webhook_claims.read().await;
        Ok(claims.contains_key(webhook_id))
    }

    async fn record_webhook_id(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let mut claims = self.webhook_claims.write().await;
        claims.insert(
            webhook_id.to_string(),
            (WebhookClaimState::Completed, Self::now_secs()),
        );
        Ok(())
    }

    async fn try_claim_webhook_id(
        &self,
        webhook_id: &str,
    ) -> Result<WebhookClaimResult, RepositoryError> {
        use std::collections::hash_map::Entry;

        let mut claims = self.webhook_claims.write().await;
        match claims.entry(webhook_id.to_string()) {
            Entry::Occupied(entry) => {
                // Already claimed - check state
                match entry.get().0 {
                    WebhookClaimState::InProgress => Ok(WebhookClaimResult::InProgress),
                    WebhookClaimState::Completed => Ok(WebhookClaimResult::Completed),
                }
            }
            Entry::Vacant(entry) => {
                // We're the first - claim it as in_progress
                entry.insert((WebhookClaimState::InProgress, Self::now_secs()));
                Ok(WebhookClaimResult::Claimed)
            }
        }
    }

    async fn complete_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let mut claims = self.webhook_claims.write().await;
        if let Some(entry) = claims.get_mut(webhook_id) {
            entry.0 = WebhookClaimState::Completed;
        }
        Ok(())
    }

    async fn release_webhook_claim(&self, webhook_id: &str) -> Result<(), RepositoryError> {
        let mut claims = self.webhook_claims.write().await;
        claims.remove(webhook_id);
        Ok(())
    }

    async fn cleanup_expired_webhooks(&self, ttl_seconds: i64) -> Result<usize, RepositoryError> {
        let now = Self::now_secs();
        let cutoff = now - ttl_seconds;

        let mut claims = self.webhook_claims.write().await;
        let initial_len = claims.len();
        claims.retain(|_, (_, timestamp)| *timestamp > cutoff);
        Ok(initial_len - claims.len())
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

    #[tokio::test]
    async fn test_get_returns_none_for_missing() {
        let repo = InMemoryRepository::new();
        let result = repo.get(&test_pr_id(1)).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_put_then_get() {
        let repo = InMemoryRepository::new();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state.clone()).await.unwrap();
        let result = repo.get(&pr_id).await.unwrap();

        assert!(result.is_some());
        let retrieved = result.unwrap();
        assert_eq!(retrieved.installation_id, Some(12345));
    }

    #[tokio::test]
    async fn test_delete() {
        let repo = InMemoryRepository::new();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state).await.unwrap();
        let deleted = repo.delete(&pr_id).await.unwrap();
        assert!(deleted.is_some());

        let after = repo.get(&pr_id).await.unwrap();
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn test_get_pending_returns_only_pending() {
        let repo = InMemoryRepository::new();

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

    /// Regression test: Cancelled states with pending_cancel_batch_id must be
    /// returned by get_pending() so that cancel-failed batches can still be polled.
    ///
    /// Bug: The original implementation filtered by has_pending_batch(), which
    /// excluded Cancelled states. This meant batches where cancel failed would
    /// never be reconciled.
    #[tokio::test]
    async fn test_get_pending_includes_cancelled_with_pending_batch() {
        let repo = InMemoryRepository::new();

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

    // =========================================================================
    // Property-based tests
    // =========================================================================

    /// Generate an arbitrary CommitSha.
    fn arb_commit_sha() -> impl Strategy<Value = CommitSha> {
        "[a-f0-9]{40}".prop_map(CommitSha)
    }

    /// Generate an arbitrary BatchId.
    fn arb_batch_id() -> impl Strategy<Value = BatchId> {
        "batch_[a-zA-Z0-9]{8}".prop_map(BatchId)
    }

    /// Generate an arbitrary CancellationReason.
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

    /// Generate an arbitrary FailureReason.
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

    /// Generate an arbitrary ReviewResult.
    fn arb_review_result() -> impl Strategy<Value = ReviewResult> {
        (any::<String>(), any::<bool>(), any::<String>()).prop_map(
            |(reasoning, substantive_comments, summary)| ReviewResult {
                reasoning,
                substantive_comments,
                summary,
            },
        )
    }

    /// Generate an arbitrary ReviewOptions.
    fn arb_review_options() -> impl Strategy<Value = ReviewOptions> {
        (any::<Option<String>>(), any::<Option<String>>()).prop_map(|(model, reasoning_effort)| {
            ReviewOptions {
                model,
                reasoning_effort,
            }
        })
    }

    /// Generate an arbitrary ReviewMachineState.
    ///
    /// This covers all state variants to ensure comprehensive testing.
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
        ///
        /// This is the key invariant that ensures:
        /// 1. Active batches (BatchPending, AwaitingAncestryCheck) are polled
        /// 2. Cancel-failed batches (Cancelled with pending_cancel_batch_id) are polled
        /// 3. Terminal states without pending batches are NOT polled
        #[test]
        fn get_pending_matches_pending_batch_id(states in proptest::collection::vec((0u64..1000, arb_review_state(), 1u64..1000000), 0..50)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = InMemoryRepository::new();

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
                // Note: multiple states with same pr_number will overwrite, so we need to
                // check what's actually in the repository
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

        /// Property: pending_batch_id().is_some() ⟺ state should be returned by get_pending()
        ///
        /// This is a more direct test of the invariant for single states.
        #[test]
        fn single_state_pending_invariant(state in arb_review_state(), installation_id in 1u64..1000000) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = InMemoryRepository::new();
                let pr_id = test_pr_id(1);
                let stored = StoredState {
                    state: state.clone(),
                    installation_id: Some(installation_id),
                };

                repo.put(&pr_id, stored).await.unwrap();

                let pending = repo.get_pending().await.unwrap();
                let has_pending_batch = state.pending_batch_id().is_some();
                let was_returned = !pending.is_empty();

                assert_eq!(
                    has_pending_batch,
                    was_returned,
                    "State has pending_batch_id: {}, but get_pending() returned: {}.\nState: {:?}",
                    has_pending_batch,
                    was_returned,
                    state
                );
            });
        }
    }

    // =========================================================================
    // Bug regression tests: Issues that need to be fixed
    // =========================================================================

    /// Test that get_submitting() returns BatchSubmitting states for crash recovery.
    ///
    /// The BatchSubmitting state is used for crash recovery - it contains a
    /// reconciliation_token that allows us to find orphaned batches at OpenAI.
    /// The get_submitting() method returns these states so they can be reconciled.
    #[tokio::test]
    async fn test_get_submitting_returns_batch_submitting() {
        let repo = InMemoryRepository::new();

        // Create a BatchSubmitting state (for crash recovery)
        let submitting_state = StoredState {
            state: ReviewMachineState::BatchSubmitting {
                reviews_enabled: true,
                reconciliation_token: "test-token-123".to_string(),
                head_sha: CommitSha::from("abc123"),
                base_sha: CommitSha::from("def456"),
                options: ReviewOptions::default(),
                comment_id: None,
                check_run_id: None,
                model: "gpt-4".to_string(),
                reasoning_effort: "high".to_string(),
            },
            installation_id: Some(111),
        };
        repo.put(&test_pr_id(1), submitting_state).await.unwrap();

        // get_pending() should NOT return BatchSubmitting states
        // (they don't have a pending batch ID yet)
        let pending = repo.get_pending().await.unwrap();
        assert_eq!(
            pending.len(),
            0,
            "get_pending() should not return BatchSubmitting states"
        );

        // get_submitting() should return BatchSubmitting states
        let submitting = repo.get_submitting().await.unwrap();
        assert_eq!(
            submitting.len(),
            1,
            "get_submitting() should return BatchSubmitting states for crash recovery"
        );
        assert_eq!(submitting[0].0.pr_number, 1);
    }

    /// Test to verify the arb_review_state() generator produces states with and without
    /// pending batches, ensuring the property tests are actually exploring both cases.
    #[test]
    fn arb_review_state_covers_both_pending_cases() {
        use proptest::strategy::ValueTree;
        use proptest::test_runner::{Config, TestRunner};

        let mut runner = TestRunner::new(Config::default());
        let mut with_pending = 0u32;
        let mut without_pending = 0u32;

        // Generate 200 states and count distribution
        for _ in 0..200 {
            let state = arb_review_state().new_tree(&mut runner).unwrap().current();
            if state.pending_batch_id().is_some() {
                with_pending += 1;
            } else {
                without_pending += 1;
            }
        }

        // Assert reasonable distribution (at least 20% of each)
        let total = with_pending + without_pending;
        let with_pending_pct = (with_pending as f64 / total as f64) * 100.0;
        let without_pending_pct = (without_pending as f64 / total as f64) * 100.0;

        assert!(
            with_pending_pct >= 20.0,
            "Generator produced too few states with pending batches: {:.1}% ({}/{})",
            with_pending_pct,
            with_pending,
            total
        );
        assert!(
            without_pending_pct >= 20.0,
            "Generator produced too few states without pending batches: {:.1}% ({}/{})",
            without_pending_pct,
            without_pending,
            total
        );
    }

    // =========================================================================
    // Webhook claim state tests
    // =========================================================================

    /// Test that try_claim_webhook_id returns Claimed for first claim.
    #[tokio::test]
    async fn test_try_claim_returns_claimed_for_first_attempt() {
        let repo = InMemoryRepository::new();

        let result = repo.try_claim_webhook_id("webhook_1").await.unwrap();
        assert_eq!(result, WebhookClaimResult::Claimed);
    }

    /// Test that try_claim_webhook_id returns InProgress for second claim
    /// before completion.
    #[tokio::test]
    async fn test_try_claim_returns_in_progress_for_concurrent_claim() {
        let repo = InMemoryRepository::new();

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
        let repo = InMemoryRepository::new();

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
        let repo = InMemoryRepository::new();

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
        let repo = InMemoryRepository::new();

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
}
