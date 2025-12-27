//! State store for per-PR state machines.
//!
//! This module provides a thread-safe store for managing state machines
//! for each pull request. It integrates with the transition function and
//! effect interpreter to handle state changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tracing::info;

use super::event::Event;
use super::interpreter::{execute_effects, InterpreterContext};
use super::state::{CommitSha, ReviewMachineState, ReviewOptions};
use super::transition::{transition, TransitionResult};
use crate::github::GitHubClient;
use crate::openai::OpenAIClient;

/// Information about a PR in the `Preparing` state, used for startup recovery.
#[derive(Debug, Clone)]
pub struct PreparingPrInfo {
    pub pr_id: StateMachinePrId,
    pub installation_id: u64,
    pub head_sha: CommitSha,
    pub base_sha: CommitSha,
    pub options: ReviewOptions,
}

/// Unique identifier for a pull request across repositories.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StateMachinePrId {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
}

impl StateMachinePrId {
    pub fn new(
        repo_owner: impl Into<String>,
        repo_name: impl Into<String>,
        pr_number: u64,
    ) -> Self {
        Self {
            repo_owner: repo_owner.into(),
            repo_name: repo_name.into(),
            pr_number,
        }
    }
}

/// Thread-safe store for per-PR state machines.
///
/// This store provides per-PR serialization to prevent race conditions when
/// concurrent events (webhooks, commands, batch completions) target the same PR.
/// All state-modifying operations acquire a per-PR lock before proceeding.
pub struct StateStore {
    states: RwLock<HashMap<StateMachinePrId, ReviewMachineState>>,
    /// Installation IDs for each PR (needed for batch polling to auth with GitHub).
    installation_ids: RwLock<HashMap<StateMachinePrId, u64>>,
    /// Per-PR locks to serialize event processing.
    ///
    /// This ensures that concurrent events for the same PR are processed
    /// sequentially, preventing races where one event reads stale state
    /// before another event's writes are visible.
    pr_locks: RwLock<HashMap<StateMachinePrId, Arc<Mutex<()>>>>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            installation_ids: RwLock::new(HashMap::new()),
            pr_locks: RwLock::new(HashMap::new()),
        }
    }

    /// Get or create a lock for a specific PR.
    ///
    /// This lock is used to serialize all state-modifying operations for a PR,
    /// preventing concurrent events from racing each other.
    async fn get_or_create_pr_lock(&self, pr_id: &StateMachinePrId) -> Arc<Mutex<()>> {
        // Fast path: check if lock already exists
        {
            let locks = self.pr_locks.read().await;
            if let Some(lock) = locks.get(pr_id) {
                return lock.clone();
            }
        }

        // Slow path: create lock (double-check after acquiring write lock)
        let mut locks = self.pr_locks.write().await;
        locks
            .entry(pr_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Get the current state for a PR, or create a default idle state.
    pub async fn get_or_default(&self, pr_id: &StateMachinePrId) -> ReviewMachineState {
        let states = self.states.read().await;
        states.get(pr_id).cloned().unwrap_or_default()
    }

    /// Get or initialize the state for a PR with the given reviews_enabled setting.
    ///
    /// If the PR doesn't have a state, creates an Idle state with the given reviews_enabled.
    /// If the PR already has a state and its reviews_enabled differs, updates it to match.
    ///
    /// This method acquires a per-PR lock to prevent races with concurrent calls.
    pub async fn get_or_init(
        &self,
        pr_id: &StateMachinePrId,
        reviews_enabled: bool,
    ) -> ReviewMachineState {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let states = self.states.read().await;
        if let Some(state) = states.get(pr_id) {
            let state = state.clone();
            drop(states);

            // If reviews_enabled changed (e.g., user edited PR description),
            // update the state to reflect the new value
            if state.reviews_enabled() != reviews_enabled {
                let updated_state = state.with_reviews_enabled(reviews_enabled);
                self.set(pr_id.clone(), updated_state.clone()).await;
                return updated_state;
            }
            return state;
        }
        drop(states);

        // Initialize with the given reviews_enabled
        let state = ReviewMachineState::Idle { reviews_enabled };
        self.set(pr_id.clone(), state.clone()).await;
        state
    }

    /// Get the current state for a PR.
    pub async fn get(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        let states = self.states.read().await;
        states.get(pr_id).cloned()
    }

    /// Set the state for a PR.
    pub async fn set(&self, pr_id: StateMachinePrId, state: ReviewMachineState) {
        let mut states = self.states.write().await;
        states.insert(pr_id, state);
    }

    /// Remove the state for a PR (e.g., when PR is closed).
    ///
    /// This method acquires a per-PR lock to ensure no concurrent operations
    /// are modifying the state while it's being removed.
    ///
    /// Note: We intentionally do NOT remove the lock entry from `pr_locks` after deletion.
    /// Removing the lock creates a race condition: if another task already cloned the Arc
    /// and is waiting on the mutex, they will proceed after we release. If we then remove
    /// the lock entry, a third task could create a NEW lock, allowing concurrent operations
    /// on the same PR. The memory overhead of keeping lock entries is negligible (one
    /// Arc<Mutex<()>> per PR ever seen).
    pub async fn remove(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let mut states = self.states.write().await;
        let mut installation_ids = self.installation_ids.write().await;
        installation_ids.remove(pr_id);
        let result = states.remove(pr_id);
        drop(states);
        drop(installation_ids);

        result
    }

    /// Set the installation ID for a PR.
    pub async fn set_installation_id(&self, pr_id: &StateMachinePrId, installation_id: u64) {
        let mut installation_ids = self.installation_ids.write().await;
        installation_ids.insert(pr_id.clone(), installation_id);
    }

    /// Get the installation ID for a PR.
    pub async fn get_installation_id(&self, pr_id: &StateMachinePrId) -> Option<u64> {
        let installation_ids = self.installation_ids.read().await;
        installation_ids.get(pr_id).copied()
    }

    /// Get all PR IDs with pending batches.
    pub async fn get_pending_pr_ids(&self) -> Vec<StateMachinePrId> {
        let states = self.states.read().await;
        states
            .iter()
            .filter(|(_, state)| state.has_pending_batch())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get all pending batches with their PR information.
    ///
    /// Returns a list of (pr_id, batch_id, installation_id) tuples for all PRs with pending batches.
    pub async fn get_pending_batches(&self) -> Vec<(StateMachinePrId, super::state::BatchId, u64)> {
        let states = self.states.read().await;
        let installation_ids = self.installation_ids.read().await;
        states
            .iter()
            .filter_map(|(id, state)| {
                let batch_id = state.pending_batch_id()?;
                let installation_id = installation_ids.get(id).copied()?;
                Some((id.clone(), batch_id.clone(), installation_id))
            })
            .collect()
    }

    /// Get all PRs in `Preparing` state with their information.
    ///
    /// Used for startup recovery: if the server crashed after persisting a PR
    /// in `Preparing` state but before/during `FetchData` execution, we need
    /// to re-drive the effect to avoid the PR being stuck indefinitely.
    pub async fn get_preparing_pr_ids(&self) -> Vec<PreparingPrInfo> {
        let states = self.states.read().await;
        let installation_ids = self.installation_ids.read().await;
        states
            .iter()
            .filter_map(|(id, state)| {
                if let ReviewMachineState::Preparing {
                    head_sha,
                    base_sha,
                    options,
                    ..
                } = state
                {
                    let installation_id = installation_ids.get(id).copied()?;
                    Some(PreparingPrInfo {
                        pr_id: id.clone(),
                        installation_id,
                        head_sha: head_sha.clone(),
                        base_sha: base_sha.clone(),
                        options: options.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Compute a transition without executing effects.
    ///
    /// This method performs the pure transition function, returning the new state
    /// and the effects that should be executed. The effects are NOT executed.
    ///
    /// This is useful for `PersistentStateStore` which needs to persist state
    /// BEFORE executing effects to ensure crash safety.
    ///
    /// Note: This method does NOT acquire the per-PR lock. The caller must
    /// ensure proper serialization.
    pub fn compute_transition(
        &self,
        pr_id: &StateMachinePrId,
        current_state: ReviewMachineState,
        event: Event,
    ) -> (ReviewMachineState, Vec<super::effect::Effect>) {
        info!(
            "Computing transition for event {} on PR #{} in state {:?}",
            event.log_summary(),
            pr_id.pr_number,
            current_state
        );

        let TransitionResult { state, effects } = transition(current_state, event);
        (state, effects)
    }

    /// Execute effects and return result events.
    ///
    /// This is the effectful part of processing, separated from the pure transition
    /// so that `PersistentStateStore` can persist state between transition and effects.
    ///
    /// Note: This method does NOT acquire the per-PR lock. The caller must
    /// ensure proper serialization.
    pub async fn execute_effects(
        &self,
        pr_id: &StateMachinePrId,
        effects: Vec<super::effect::Effect>,
        ctx: &InterpreterContext,
    ) -> Vec<Event> {
        if !effects.is_empty() {
            info!(
                "Executing {} effects for PR #{}",
                effects.len(),
                pr_id.pr_number
            );
            execute_effects(ctx, effects).await
        } else {
            vec![]
        }
    }

    /// Process a single event step: transition and execute effects.
    ///
    /// This method performs ONE transition and executes its effects, returning
    /// the new state and any result events generated by the effects.
    ///
    /// This is a building block for `process_event` and allows callers (like
    /// `PersistentStateStore`) to inject persistence between steps.
    ///
    /// Note: This method does NOT store the resulting state. The caller is
    /// responsible for storing the state after each step if desired.
    ///
    /// Note: This method does NOT acquire the per-PR lock. The caller must
    /// ensure proper serialization.
    pub async fn process_event_step(
        &self,
        pr_id: &StateMachinePrId,
        current_state: ReviewMachineState,
        event: Event,
        ctx: &InterpreterContext,
    ) -> (ReviewMachineState, Vec<Event>) {
        let (state, effects) = self.compute_transition(pr_id, current_state, event);
        let result_events = self.execute_effects(pr_id, effects, ctx).await;
        (state, result_events)
    }

    /// Process an event for a PR: transition the state and execute effects.
    ///
    /// This is the main entry point for handling events. It:
    /// 1. Acquires a per-PR lock to serialize with concurrent events
    /// 2. Gets (or creates) the current state
    /// 3. Runs the transition function
    /// 4. Executes effects via the interpreter
    /// 5. Handles result events recursively
    /// 6. Stores the final state
    ///
    /// The per-PR lock ensures that concurrent events for the same PR are
    /// processed sequentially, preventing races where one event reads stale
    /// state or overwrites another event's changes.
    ///
    /// Returns the final state after all transitions.
    pub async fn process_event(
        &self,
        pr_id: &StateMachinePrId,
        event: Event,
        ctx: &InterpreterContext,
    ) -> ReviewMachineState {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        // Store the installation_id for later use by batch polling
        self.set_installation_id(pr_id, ctx.installation_id).await;

        let mut current_state = self.get_or_default(pr_id).await;

        // Event loop: process initial event and any result events from effects
        let mut events_to_process = vec![event];

        while let Some(event) = events_to_process.pop() {
            let (new_state, result_events) = self
                .process_event_step(pr_id, current_state, event, ctx)
                .await;
            current_state = new_state;

            // Add result events to be processed (in reverse order so they're processed in order)
            for result_event in result_events.into_iter().rev() {
                events_to_process.push(result_event);
            }
        }

        // Store the final state
        self.set(pr_id.clone(), current_state.clone()).await;

        info!(
            "Final state for PR #{}: {:?}",
            pr_id.pr_number, current_state
        );

        current_state
    }
}

/// Context for running the state machine.
///
/// This wraps the InterpreterContext with PR-specific information.
pub struct StateMachineContext {
    pub github_client: Arc<GitHubClient>,
    pub openai_client: Arc<OpenAIClient>,
    pub db: Arc<crate::db::SqliteDb>,
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub pr_url: Option<String>,
    pub branch_name: Option<String>,
    pub correlation_id: Option<String>,
}

impl StateMachineContext {
    /// Create an InterpreterContext from this context.
    pub fn to_interpreter_context(&self) -> InterpreterContext {
        InterpreterContext {
            github_client: self.github_client.clone(),
            openai_client: self.openai_client.clone(),
            db: self.db.clone(),
            installation_id: self.installation_id,
            repo_owner: self.repo_owner.clone(),
            repo_name: self.repo_name.clone(),
            pr_number: self.pr_number,
            pr_url: self.pr_url.clone(),
            branch_name: self.branch_name.clone(),
            correlation_id: self.correlation_id.clone(),
        }
    }
}

// =============================================================================
// Webhook-to-Event Conversion Helpers
// =============================================================================

/// Convert a PR update (opened/synchronized/edited) webhook to an Event.
pub fn pr_updated_event(
    head_sha: impl Into<String>,
    base_sha: impl Into<String>,
    force_review: bool,
    options: Option<crate::command::ReviewOptions>,
) -> Event {
    Event::PrUpdated {
        head_sha: CommitSha::from(head_sha.into()),
        base_sha: CommitSha::from(base_sha.into()),
        force_review,
        options: options.map(ReviewOptions::from).unwrap_or_default(),
    }
}

/// Convert a manual review request command to an Event.
pub fn review_requested_event(
    head_sha: impl Into<String>,
    base_sha: impl Into<String>,
    options: Option<crate::command::ReviewOptions>,
) -> Event {
    Event::ReviewRequested {
        head_sha: CommitSha::from(head_sha.into()),
        base_sha: CommitSha::from(base_sha.into()),
        options: options.map(ReviewOptions::from).unwrap_or_default(),
    }
}

/// Create a cancel requested event.
pub fn cancel_requested_event() -> Event {
    Event::CancelRequested
}

/// Create an enable reviews requested event.
pub fn enable_reviews_event(
    head_sha: impl Into<String>,
    base_sha: impl Into<String>,
    options: Option<crate::command::ReviewOptions>,
) -> Event {
    Event::EnableReviewsRequested {
        head_sha: CommitSha::from(head_sha.into()),
        base_sha: CommitSha::from(base_sha.into()),
        options: options.map(ReviewOptions::from).unwrap_or_default(),
    }
}

/// Create a disable reviews requested event.
pub fn disable_reviews_event() -> Event {
    Event::DisableReviewsRequested
}

/// Create a batch completed event from a successful batch response.
pub fn batch_completed_event(
    batch_id: impl Into<String>,
    result: super::state::ReviewResult,
) -> Event {
    Event::BatchCompleted {
        batch_id: super::state::BatchId::from(batch_id.into()),
        result,
    }
}

/// Create a batch terminated event from a failed/expired/cancelled batch.
pub fn batch_terminated_event(
    batch_id: impl Into<String>,
    reason: super::state::FailureReason,
) -> Event {
    Event::BatchTerminated {
        batch_id: super::state::BatchId::from(batch_id.into()),
        reason,
    }
}

/// Create a batch status update event (for batches still processing).
pub fn batch_status_update_event(
    batch_id: impl Into<String>,
    status: super::event::BatchStatus,
) -> Event {
    Event::BatchStatusUpdate {
        batch_id: super::state::BatchId::from(batch_id.into()),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_state_store_get_or_default() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        let state = store.get_or_default(&pr_id).await;
        assert!(matches!(
            state,
            ReviewMachineState::Idle {
                reviews_enabled: true
            }
        ));
    }

    #[tokio::test]
    async fn test_state_store_set_and_get() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        store.set(pr_id.clone(), state.clone()).await;

        let retrieved = store.get(&pr_id).await;
        assert_eq!(retrieved, Some(state));
    }

    #[tokio::test]
    async fn test_state_store_remove() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        store.set(pr_id.clone(), state.clone()).await;

        let removed = store.remove(&pr_id).await;
        assert_eq!(removed, Some(state));

        let after_remove = store.get(&pr_id).await;
        assert_eq!(after_remove, None);
    }

    /// Regression test: get_or_init should update reviews_enabled when it differs
    /// from the rehydrated value.
    ///
    /// Bug: When a PR description is edited to add/remove the "no review" marker,
    /// the rehydrated reviews_enabled value should update the existing state.
    /// Previously, get_or_init ignored the reviews_enabled parameter if a state
    /// already existed.
    #[tokio::test]
    async fn test_get_or_init_updates_reviews_enabled_when_changed() {
        use crate::state_machine::state::{BatchId, CheckRunId, CommentId, CommitSha};

        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Simulate: PR opened with reviews enabled, batch is pending
        let initial_state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        store.set(pr_id.clone(), initial_state).await;

        // Now rehydrate with reviews_enabled: false (user added <!-- no review -->)
        let state = store.get_or_init(&pr_id, false).await;

        // The returned state should have reviews_enabled: false
        // (Bug: previously it returned true because it didn't update existing state)
        assert!(
            !state.reviews_enabled(),
            "get_or_init should update reviews_enabled to match rehydrated value"
        );
    }

    #[tokio::test]
    async fn test_get_or_init_creates_new_state_with_reviews_enabled() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // No existing state - should create Idle with reviews_enabled: false
        let state = store.get_or_init(&pr_id, false).await;

        assert!(matches!(
            state,
            ReviewMachineState::Idle {
                reviews_enabled: false
            }
        ));
    }

    /// Test that concurrent get_or_init calls for the same PR are serialized.
    ///
    /// This test spawns multiple concurrent tasks that each call get_or_init
    /// for the same PR. The per-PR lock should ensure they're serialized,
    /// preventing races where one task's update overwrites another's.
    #[tokio::test]
    async fn test_concurrent_get_or_init_is_serialized() {
        use std::sync::Arc;

        let store = Arc::new(StateStore::new());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Spawn 10 concurrent tasks that alternate between enabling/disabling reviews
        let mut handles = vec![];
        for i in 0..10 {
            let store = store.clone();
            let pr_id = pr_id.clone();
            let reviews_enabled = i % 2 == 0;
            handles.push(tokio::spawn(async move {
                store.get_or_init(&pr_id, reviews_enabled).await
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            let _ = handle.await.unwrap();
        }

        // The final state should exist and be consistent
        let final_state = store.get(&pr_id).await;
        assert!(
            final_state.is_some(),
            "State should exist after concurrent get_or_init calls"
        );
    }

    /// Test that concurrent operations on different PRs can proceed in parallel.
    ///
    /// This verifies that the per-PR locking doesn't accidentally serialize
    /// operations across different PRs.
    #[tokio::test]
    async fn test_different_prs_not_blocked() {
        use std::sync::Arc;

        let store = Arc::new(StateStore::new());

        // Spawn tasks for different PRs concurrently
        let mut handles = vec![];
        for pr_number in 1..=5 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let pr_id = StateMachinePrId::new("owner", "repo", pr_number);
                store.get_or_init(&pr_id, true).await
            }));
        }

        // All should complete without blocking each other
        for handle in handles {
            let state = handle.await.unwrap();
            assert!(state.reviews_enabled());
        }
    }
}
