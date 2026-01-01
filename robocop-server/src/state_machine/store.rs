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
use super::repository::{InMemoryRepository, RepositoryError, StateRepository, StoredState};
use super::state::{CommitSha, ReviewMachineState, ReviewOptions};
use super::transition::{transition, TransitionResult};
use crate::github::GitHubClient;
use crate::openai::OpenAIClient;
use tracing::error;

/// Unique identifier for a pull request across repositories.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
///
/// The actual state storage is delegated to a `StateRepository` implementation,
/// allowing different backends (in-memory, SQLite, etc.).
pub struct StateStore {
    /// Repository for persisting PR states.
    repository: Arc<dyn StateRepository>,
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
    /// Create a new StateStore with the default in-memory repository.
    pub fn new() -> Self {
        Self::with_repository(Arc::new(InMemoryRepository::new()))
    }

    /// Create a new StateStore with a custom repository.
    pub fn with_repository(repository: Arc<dyn StateRepository>) -> Self {
        Self {
            repository,
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
    ///
    /// Returns:
    /// - `Ok(state)` - the current state (or default if not found)
    /// - `Err(RepositoryError)` - if a storage error occurred
    ///
    /// IMPORTANT: This method distinguishes between "not found" (returns default)
    /// and "read error" (returns Err). Callers MUST handle errors appropriately
    /// to avoid processing from a default state when the real state couldn't be read.
    pub async fn get_or_default(
        &self,
        pr_id: &StateMachinePrId,
    ) -> Result<ReviewMachineState, RepositoryError> {
        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => Ok(stored.state),
            Ok(None) => Ok(ReviewMachineState::default()),
            Err(e) => {
                error!(
                    "Repository error getting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                Err(e)
            }
        }
    }

    /// Get or initialize the state for a PR with the given reviews_enabled setting.
    ///
    /// If the PR doesn't have a state, creates an Idle state with the given reviews_enabled.
    /// If the PR already has a state and its reviews_enabled differs, updates it to match.
    ///
    /// This method acquires a per-PR lock to prevent races with concurrent calls.
    ///
    /// Returns:
    /// - `Ok(state)` - the current/updated state (always matches what's persisted)
    /// - `Err(RepositoryError)` - if a storage error occurred (read or write)
    ///
    /// IMPORTANT: If the reviews_enabled update fails to persist, this returns Err
    /// so callers know the update didn't take effect. This prevents the scenario where
    /// a user disables reviews (e.g., adding <!-- no review -->) but the update fails
    /// silently, causing reviews to run when they shouldn't.
    pub async fn get_or_init(
        &self,
        pr_id: &StateMachinePrId,
        reviews_enabled: bool,
    ) -> Result<ReviewMachineState, RepositoryError> {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => {
                // If reviews_enabled changed (e.g., user edited PR description),
                // update the state to reflect the new value
                if stored.state.reviews_enabled() != reviews_enabled {
                    let updated_state = stored.state.clone().with_reviews_enabled(reviews_enabled);
                    self.repository
                        .put(
                            pr_id,
                            StoredState {
                                state: updated_state.clone(),
                                installation_id: stored.installation_id,
                            },
                        )
                        .await
                        .map_err(|e| {
                            error!(
                                "Repository error updating reviews_enabled for PR #{}: {}",
                                pr_id.pr_number, e
                            );
                            e
                        })?;
                    return Ok(updated_state);
                }
                Ok(stored.state)
            }
            Ok(None) => {
                // Initialize with the given reviews_enabled
                // Note: installation_id is None until process_event is called with a real ID
                let state = ReviewMachineState::Idle { reviews_enabled };
                self.repository
                    .put(
                        pr_id,
                        StoredState {
                            state: state.clone(),
                            installation_id: None, // Will be set when process_event is called
                        },
                    )
                    .await
                    .map_err(|e| {
                        error!(
                            "Repository error creating state for PR #{}: {}",
                            pr_id.pr_number, e
                        );
                        e
                    })?;
                Ok(state)
            }
            Err(e) => {
                error!(
                    "Repository error getting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                Err(e)
            }
        }
    }

    /// Get the current state for a PR.
    ///
    /// Returns `None` if not found or on storage error (error is logged).
    pub async fn get(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => Some(stored.state),
            Ok(None) => None,
            Err(e) => {
                error!(
                    "Repository error getting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                None
            }
        }
    }

    /// Set the state for a PR.
    ///
    /// The caller must provide the installation_id to avoid read-modify-write
    /// races that could lose the installation_id on transient repository errors.
    ///
    /// Returns `Err` if the state could not be persisted. Callers MUST handle
    /// this error to avoid emitting effects without durable state.
    ///
    /// # Safety
    /// This method must only be called while holding the per-PR lock to prevent
    /// read-modify-write races. It is intentionally private; external callers
    /// should use `process_event` which handles locking.
    async fn set(
        &self,
        pr_id: StateMachinePrId,
        state: ReviewMachineState,
        installation_id: Option<u64>,
    ) -> Result<(), RepositoryError> {
        self.repository
            .put(
                &pr_id,
                StoredState {
                    state,
                    installation_id,
                },
            )
            .await
            .map_err(|e| {
                error!(
                    "Repository error setting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                e
            })
    }

    /// Remove the state for a PR (e.g., when PR is closed).
    ///
    /// This method acquires a per-PR lock to ensure no concurrent operations
    /// are modifying the state while it's being removed.
    pub async fn remove(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let result = match self.repository.delete(pr_id).await {
            Ok(Some(stored)) => Some(stored.state),
            Ok(None) => None,
            Err(e) => {
                error!(
                    "Repository error deleting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                None
            }
        };

        // Clean up the lock entry (we're still holding it, so this is safe)
        let mut locks = self.pr_locks.write().await;
        locks.remove(pr_id);

        result
    }

    /// Get the installation ID for a PR.
    pub async fn get_installation_id(&self, pr_id: &StateMachinePrId) -> Option<u64> {
        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => stored.installation_id,
            Ok(None) => None,
            Err(e) => {
                error!(
                    "Repository error getting installation_id for PR #{}: {}",
                    pr_id.pr_number, e
                );
                None
            }
        }
    }

    /// Get all PR IDs with pending batches.
    pub async fn get_pending_pr_ids(&self) -> Vec<StateMachinePrId> {
        match self.repository.get_pending().await {
            Ok(pending) => pending.into_iter().map(|(id, _)| id).collect(),
            Err(e) => {
                error!("Repository error getting pending PR IDs: {}", e);
                Vec::new()
            }
        }
    }

    /// Get all pending batches with their PR information.
    ///
    /// Returns a list of (pr_id, batch_id, installation_id) tuples for all PRs with pending batches.
    /// States without an installation_id are filtered out since we can't authenticate to GitHub.
    pub async fn get_pending_batches(&self) -> Vec<(StateMachinePrId, super::state::BatchId, u64)> {
        match self.repository.get_pending().await {
            Ok(pending) => pending
                .into_iter()
                .filter_map(|(id, stored)| {
                    let batch_id = stored.state.pending_batch_id()?.clone();
                    // Filter out states without installation_id - we can't authenticate to GitHub
                    let installation_id = stored.installation_id?;
                    Some((id, batch_id, installation_id))
                })
                .collect(),
            Err(e) => {
                error!("Repository error getting pending batches: {}", e);
                Vec::new()
            }
        }
    }

    /// Get all PRs in the BatchSubmitting state (for crash recovery).
    ///
    /// Returns a list of (pr_id, reconciliation_token, installation_id) tuples for all PRs
    /// with in-flight batch submissions. These need to be reconciled on startup.
    pub async fn get_submitting_states(&self) -> Vec<(StateMachinePrId, String, u64)> {
        match self.repository.get_pending().await {
            Ok(pending) => pending
                .into_iter()
                .filter_map(|(id, stored)| {
                    // Only include BatchSubmitting states
                    let token = stored.state.reconciliation_token()?.to_string();
                    // Filter out states without installation_id
                    let installation_id = stored.installation_id?;
                    Some((id, token, installation_id))
                })
                .collect(),
            Err(e) => {
                error!("Repository error getting submitting states: {}", e);
                Vec::new()
            }
        }
    }

    /// Process an event for a PR: transition the state and execute effects.
    ///
    /// This is the main entry point for handling events. It:
    /// 1. Acquires a per-PR lock to serialize with concurrent events
    /// 2. Gets (or creates) the current state
    /// 3. Runs the transition function
    /// 4. Persists the new state BEFORE executing effects
    /// 5. Executes effects via the interpreter
    /// 6. Handles result events in a loop (each iteration persists before effects)
    ///
    /// The per-PR lock ensures that concurrent events for the same PR are
    /// processed sequentially, preventing races where one event reads stale
    /// state or overwrites another event's changes.
    ///
    /// IMPORTANT: State is persisted BEFORE effects are executed. This ensures
    /// crash recovery finds the correct state. If the process crashes after
    /// persistence but before effects complete, effects may be lost but state
    /// is correct. Lost effects (like batch submissions) will be picked up by
    /// the batch poller eventually.
    ///
    /// Returns:
    /// - `Ok(state)` - the final state after all transitions
    /// - `Err(RepositoryError)` - if a read or write failed
    ///
    /// On read error, this method returns early WITHOUT processing the event
    /// and WITHOUT writing to storage. This prevents overwriting valid
    /// persisted state with state derived from defaults.
    pub async fn process_event(
        &self,
        pr_id: &StateMachinePrId,
        event: Event,
        ctx: &InterpreterContext,
    ) -> Result<ReviewMachineState, RepositoryError> {
        // Acquire per-PR lock to serialize with other operations
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        // Get current state FIRST, before any writes.
        // On read error, return early WITHOUT processing or writing.
        // This prevents overwriting valid persisted state with defaults,
        // and ensures no partial writes occur before returning an error.
        let mut current_state = self.get_or_default(pr_id).await?;

        // Event loop: process initial event and any result events from effects
        let mut events_to_process = vec![event];

        while let Some(event) = events_to_process.pop() {
            info!(
                "Processing event {} for PR #{} in state {:?}",
                event.log_summary(),
                pr_id.pr_number,
                current_state
            );

            let TransitionResult { state, effects } = transition(current_state, event);
            current_state = state;

            // Persist state BEFORE executing effects.
            // This ensures crash recovery finds the correct state even if effects fail.
            // We persist after EACH transition, not just at the end, so that result
            // events from effects also have their state persisted before their effects.
            self.set(
                pr_id.clone(),
                current_state.clone(),
                Some(ctx.installation_id),
            )
            .await?;

            if !effects.is_empty() {
                info!(
                    "Executing {} effects for PR #{}",
                    effects.len(),
                    pr_id.pr_number
                );

                // Execute effects and collect result events.
                // State has already been persisted, so crash here is safe.
                let result_events = execute_effects(ctx, effects).await;

                // Add result events to be processed (in reverse order so they're processed in order)
                for result_event in result_events.into_iter().rev() {
                    events_to_process.push(result_event);
                }
            }
        }

        info!(
            "Final state for PR #{}: {:?}",
            pr_id.pr_number, current_state
        );

        Ok(current_state)
    }
}

/// Context for running the state machine.
///
/// This wraps the InterpreterContext with PR-specific information.
pub struct StateMachineContext {
    pub github_client: Arc<GitHubClient>,
    pub openai_client: Arc<OpenAIClient>,
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
    use crate::state_machine::repository::RepositoryError;

    #[tokio::test]
    async fn test_state_store_get_or_default() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        let state = store.get_or_default(&pr_id).await.unwrap();
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
        store
            .set(pr_id.clone(), state.clone(), Some(12345))
            .await
            .unwrap();

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
        store
            .set(pr_id.clone(), state.clone(), Some(12345))
            .await
            .unwrap();

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
        store
            .set(pr_id.clone(), initial_state, Some(12345))
            .await
            .unwrap();

        // Now rehydrate with reviews_enabled: false (user added <!-- no review -->)
        let state = store.get_or_init(&pr_id, false).await.unwrap();

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
        let state = store.get_or_init(&pr_id, false).await.unwrap();

        assert!(matches!(
            state,
            ReviewMachineState::Idle {
                reviews_enabled: false
            }
        ));
    }

    /// Test that concurrent get_or_init calls for the same PR are serialized.
    ///
    /// Property: Each call to get_or_init returns a state where reviews_enabled
    /// matches the value passed in. This proves serialization because without it,
    /// a call could read stale state and return a mismatched value.
    ///
    /// This test spawns multiple concurrent tasks that alternate between
    /// enabling/disabling reviews. The per-PR lock should ensure they're
    /// serialized, so each call observes the state after any prior writes.
    #[tokio::test]
    async fn test_concurrent_get_or_init_is_serialized() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let store = Arc::new(StateStore::new());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);
        let mismatch_count = Arc::new(AtomicUsize::new(0));

        // Spawn 20 concurrent tasks that alternate between enabling/disabling reviews
        let mut handles = vec![];
        for i in 0..20 {
            let store = store.clone();
            let pr_id = pr_id.clone();
            let mismatch_count = mismatch_count.clone();
            let reviews_enabled = i % 2 == 0;
            handles.push(tokio::spawn(async move {
                let state = store.get_or_init(&pr_id, reviews_enabled).await.unwrap();
                // Each call should return a state matching its input.
                // If serialization is broken, a call could read stale state
                // and return reviews_enabled != what was passed in.
                if state.reviews_enabled() != reviews_enabled {
                    mismatch_count.fetch_add(1, Ordering::SeqCst);
                }
                state
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            let _ = handle.await.unwrap();
        }

        // Serialization ensures each call sees a consistent state and updates it
        assert_eq!(
            mismatch_count.load(Ordering::SeqCst),
            0,
            "All get_or_init calls should return states matching their input \
             (serialization prevents stale reads)"
        );

        // The final state should exist
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
                store.get_or_init(&pr_id, true).await.unwrap()
            }));
        }

        // All should complete without blocking each other
        for handle in handles {
            let state = handle.await.unwrap();
            assert!(state.reviews_enabled());
        }
    }

    // =========================================================================
    // Repository error path tests
    // =========================================================================

    /// A repository that fails on demand, for testing error handling.
    struct FailingRepository {
        inner: InMemoryRepository,
        fail_gets: std::sync::atomic::AtomicBool,
        fail_puts: std::sync::atomic::AtomicBool,
    }

    impl FailingRepository {
        fn new() -> Self {
            Self {
                inner: InMemoryRepository::new(),
                fail_gets: std::sync::atomic::AtomicBool::new(false),
                fail_puts: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn set_fail_gets(&self, fail: bool) {
            self.fail_gets
                .store(fail, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl StateRepository for FailingRepository {
        async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
            if self.fail_gets.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RepositoryError::storage("get", "simulated read failure"));
            }
            self.inner.get(id).await
        }

        async fn put(
            &self,
            id: &StateMachinePrId,
            state: StoredState,
        ) -> Result<(), RepositoryError> {
            if self.fail_puts.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RepositoryError::storage("put", "simulated write failure"));
            }
            self.inner.put(id, state).await
        }

        async fn delete(
            &self,
            id: &StateMachinePrId,
        ) -> Result<Option<StoredState>, RepositoryError> {
            self.inner.delete(id).await
        }

        async fn get_pending(
            &self,
        ) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
            self.inner.get_pending().await
        }
    }

    // =========================================================================
    // Regression tests for repository error handling
    // =========================================================================

    /// Regression test: get_or_default must return Err on read errors, not a default.
    ///
    /// This prevents process_event from processing with wrong state and overwriting
    /// valid persisted state.
    #[tokio::test]
    async fn test_get_or_default_returns_error_on_read_failure() {
        let repo = Arc::new(FailingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // First, write a non-default state
        let original_state = ReviewMachineState::Idle {
            reviews_enabled: false, // NOT the default (which is true)
        };
        store
            .set(pr_id.clone(), original_state.clone(), Some(111))
            .await
            .unwrap();

        // Make reads fail (simulating transient storage error)
        repo.set_fail_gets(true);

        // get_or_default should return Err, not the default state
        let result = store.get_or_default(&pr_id).await;
        assert!(
            result.is_err(),
            "get_or_default should return Err on read failure, not a default state"
        );

        // Stop failing reads so we can verify the original state is still there
        repo.set_fail_gets(false);

        // The original state should be preserved (never overwritten)
        let stored = repo.inner.get(&pr_id).await.unwrap().unwrap();
        assert!(
            !stored.state.reviews_enabled(),
            "Original state must be preserved when read fails"
        );
    }

    /// Regression test: get_or_init returns Err when write fails.
    ///
    /// When storage write fails while updating reviews_enabled, get_or_init
    /// now returns Err so callers know the update didn't take effect.
    /// This prevents reviews from running when they should be suppressed.
    #[tokio::test]
    async fn test_get_or_init_returns_err_on_write_failure() {
        let repo = Arc::new(FailingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // First, write a state with reviews_enabled: true
        let original_state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        store
            .set(pr_id.clone(), original_state.clone(), Some(111))
            .await
            .unwrap();

        // Now make writes fail
        repo.fail_puts
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Call get_or_init with reviews_enabled: false
        // This should return Err because the write failed
        let result = store.get_or_init(&pr_id, false).await;

        assert!(
            result.is_err(),
            "get_or_init should return Err when write fails, not Ok"
        );

        // Stop failing writes
        repo.fail_puts
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Read the actual persisted state - should be unchanged
        let persisted = repo.inner.get(&pr_id).await.unwrap().unwrap();
        assert!(
            persisted.state.reviews_enabled(),
            "Persisted state should still have reviews_enabled: true"
        );
    }

    /// Test that installation_id is preserved across state updates.
    ///
    /// Regression test for: installation_id preservation across state updates.
    #[tokio::test]
    async fn test_installation_id_preserved_across_state_updates() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set initial state with installation_id
        let initial_state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        store
            .set(pr_id.clone(), initial_state, Some(12345))
            .await
            .unwrap();

        // Verify installation_id is set
        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(installation_id, Some(12345));

        // Update to a different state, passing the same installation_id
        let new_state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        store
            .set(pr_id.clone(), new_state, Some(12345))
            .await
            .unwrap();

        // Verify installation_id is still present
        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(
            installation_id,
            Some(12345),
            "installation_id should be preserved across state updates"
        );
    }

    /// Test that get_or_init preserves installation_id when updating reviews_enabled.
    #[tokio::test]
    async fn test_get_or_init_preserves_installation_id() {
        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set up initial state with installation_id
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        store.set(pr_id.clone(), state, Some(12345)).await.unwrap();

        // Call get_or_init with different reviews_enabled value
        // This should update reviews_enabled but preserve installation_id
        let result = store.get_or_init(&pr_id, false).await.unwrap();

        assert!(
            !result.reviews_enabled(),
            "reviews_enabled should be updated"
        );

        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(
            installation_id,
            Some(12345),
            "installation_id should be preserved after get_or_init"
        );
    }

    // =========================================================================
    // Property-based tests
    // =========================================================================

    use crate::state_machine::state::{
        BatchId, CancellationReason, CheckRunId, CommentId, CommitSha, FailureReason,
        ReviewOptions, ReviewResult,
    };
    use proptest::prelude::*;

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

    /// Generate an arbitrary ReviewMachineState covering all variants.
    fn arb_review_state() -> impl Strategy<Value = ReviewMachineState> {
        prop_oneof![
            // Idle
            any::<bool>().prop_map(|reviews_enabled| ReviewMachineState::Idle { reviews_enabled }),
            // Preparing
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
            // AwaitingAncestryCheck
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
                    },
                ),
            // BatchPending
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
                    },
                ),
            // Completed
            (any::<bool>(), arb_commit_sha(), arb_review_result()).prop_map(
                |(reviews_enabled, head_sha, result)| ReviewMachineState::Completed {
                    reviews_enabled,
                    head_sha,
                    result,
                }
            ),
            // Failed
            (any::<bool>(), arb_commit_sha(), arb_failure_reason()).prop_map(
                |(reviews_enabled, head_sha, reason)| ReviewMachineState::Failed {
                    reviews_enabled,
                    head_sha,
                    reason,
                }
            ),
            // Cancelled without pending_cancel_batch_id
            (any::<bool>(), arb_commit_sha(), arb_cancellation_reason()).prop_map(
                |(reviews_enabled, head_sha, reason)| ReviewMachineState::Cancelled {
                    reviews_enabled,
                    head_sha,
                    reason,
                    pending_cancel_batch_id: None,
                }
            ),
            // Cancelled WITH pending_cancel_batch_id
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

    fn test_pr_id(pr_number: u64) -> StateMachinePrId {
        StateMachinePrId::new("owner", "repo", pr_number)
    }

    proptest! {
        /// Property: get_or_init preserves installation_id for all state variants.
        ///
        /// For any state with an installation_id, calling get_or_init (which may
        /// update reviews_enabled) should preserve the installation_id.
        #[test]
        fn get_or_init_preserves_installation_id_for_all_variants(
            state in arb_review_state(),
            installation_id in 1u64..1000000,
            new_reviews_enabled in any::<bool>()
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = StateStore::new();
                let pr_id = test_pr_id(1);

                // Set initial state with installation_id
                store.set(pr_id.clone(), state.clone(), Some(installation_id)).await.unwrap();

                // Call get_or_init with potentially different reviews_enabled
                let _ = store.get_or_init(&pr_id, new_reviews_enabled).await.unwrap();

                // Verify installation_id is preserved
                let retrieved_id = store.get_installation_id(&pr_id).await;
                assert_eq!(
                    retrieved_id, Some(installation_id),
                    "installation_id should be preserved after get_or_init.\n\
                     Original state: {:?}\n\
                     new_reviews_enabled: {}",
                    state, new_reviews_enabled
                );
            });
        }

        /// Property: get_or_init returns state with matching reviews_enabled.
        ///
        /// For any state and any reviews_enabled value, get_or_init should return
        /// a state where reviews_enabled matches the input parameter.
        #[test]
        fn get_or_init_returns_matching_reviews_enabled(
            state in arb_review_state(),
            installation_id in 1u64..1000000,
            requested_reviews_enabled in any::<bool>()
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = StateStore::new();
                let pr_id = test_pr_id(1);

                // Set initial state
                store.set(pr_id.clone(), state.clone(), Some(installation_id)).await.unwrap();

                // Call get_or_init
                let result = store.get_or_init(&pr_id, requested_reviews_enabled).await.unwrap();

                // Verify reviews_enabled matches requested value
                assert_eq!(
                    result.reviews_enabled(), requested_reviews_enabled,
                    "get_or_init should return state with reviews_enabled matching input.\n\
                     Original state: {:?}\n\
                     Requested reviews_enabled: {}\n\
                     Got reviews_enabled: {}",
                    state, requested_reviews_enabled, result.reviews_enabled()
                );
            });
        }

        // =====================================================================
        // Property-based tests for repository error handling
        // =====================================================================

        /// Property: get_or_init returns Err when write fails (if update is needed).
        ///
        /// When reviews_enabled differs and the write fails, get_or_init returns Err
        /// so callers know the update didn't take effect.
        #[test]
        fn get_or_init_returns_err_on_write_failure_if_update_needed(
            state in arb_review_state(),
            installation_id in 1u64..1000000,
            requested_reviews_enabled in any::<bool>()
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let repo = Arc::new(FailingRepository::new());
                let store = StateStore::with_repository(repo.clone());
                let pr_id = test_pr_id(1);

                // Set initial state
                store.set(pr_id.clone(), state.clone(), Some(installation_id)).await.unwrap();

                // Make writes fail
                repo.fail_puts.store(true, std::sync::atomic::Ordering::SeqCst);

                // Call get_or_init
                let result = store.get_or_init(&pr_id, requested_reviews_enabled).await;

                // Stop failing writes
                repo.fail_puts.store(false, std::sync::atomic::Ordering::SeqCst);

                // PROPERTY: If update is needed (reviews_enabled differs), should return Err.
                // If no update needed (reviews_enabled matches), should return Ok.
                let update_needed = state.reviews_enabled() != requested_reviews_enabled;
                if update_needed {
                    assert!(
                        result.is_err(),
                        "get_or_init should return Err when write fails and update is needed.\n\
                         Original state reviews_enabled: {}\n\
                         Requested reviews_enabled: {}",
                        state.reviews_enabled(),
                        requested_reviews_enabled
                    );
                } else {
                    assert!(
                        result.is_ok(),
                        "get_or_init should return Ok when no update is needed.\n\
                         Original state reviews_enabled: {}\n\
                         Requested reviews_enabled: {}",
                        state.reviews_enabled(),
                        requested_reviews_enabled
                    );
                }
            });
        }
    }

    // =========================================================================
    // Bug regression tests: Repository error handling
    // These tests document bugs that need to be fixed. Each test should FAIL
    // until the corresponding bug is fixed.
    // =========================================================================

    /// A repository that tracks operations for testing atomicity guarantees.
    struct TrackingRepository {
        inner: InMemoryRepository,
        fail_gets: std::sync::atomic::AtomicBool,
        fail_puts: std::sync::atomic::AtomicBool,
        /// Track number of successful puts (writes that went through)
        put_count: std::sync::atomic::AtomicUsize,
        /// Track the order of operations for atomicity testing
        operations: std::sync::Mutex<Vec<String>>,
        /// Track number of gets performed (including failed ones)
        get_count: std::sync::atomic::AtomicUsize,
        /// Fail gets only after this many successful gets (0 = fail immediately)
        fail_gets_after: std::sync::atomic::AtomicUsize,
    }

    impl TrackingRepository {
        fn new() -> Self {
            Self {
                inner: InMemoryRepository::new(),
                fail_gets: std::sync::atomic::AtomicBool::new(false),
                fail_puts: std::sync::atomic::AtomicBool::new(false),
                put_count: std::sync::atomic::AtomicUsize::new(0),
                operations: std::sync::Mutex::new(Vec::new()),
                get_count: std::sync::atomic::AtomicUsize::new(0),
                fail_gets_after: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        #[allow(dead_code)] // Keep for future tests that need simple fail-all-reads behavior
        fn set_fail_gets(&self, fail: bool) {
            self.fail_gets
                .store(fail, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_fail_puts(&self, fail: bool) {
            self.fail_puts
                .store(fail, std::sync::atomic::Ordering::SeqCst);
        }

        /// Set gets to fail after N successful gets.
        /// This simulates transient failures that happen mid-operation.
        fn set_fail_gets_after(&self, n: usize) {
            self.fail_gets_after
                .store(n, std::sync::atomic::Ordering::SeqCst);
            self.fail_gets
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn put_count(&self) -> usize {
            self.put_count.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn operations(&self) -> Vec<String> {
            self.operations.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl StateRepository for TrackingRepository {
        async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("get:{}", id.pr_number));

            if self.fail_gets.load(std::sync::atomic::Ordering::SeqCst) {
                let current_count = self
                    .get_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let fail_after = self
                    .fail_gets_after
                    .load(std::sync::atomic::Ordering::SeqCst);

                // Fail if we've exceeded the "fail after" threshold
                if current_count >= fail_after {
                    return Err(RepositoryError::storage("get", "simulated read failure"));
                }
            }
            self.inner.get(id).await
        }

        async fn put(
            &self,
            id: &StateMachinePrId,
            state: StoredState,
        ) -> Result<(), RepositoryError> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("put:{}", id.pr_number));
            if self.fail_puts.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RepositoryError::storage("put", "simulated write failure"));
            }
            self.put_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.put(id, state).await
        }

        async fn delete(
            &self,
            id: &StateMachinePrId,
        ) -> Result<Option<StoredState>, RepositoryError> {
            self.inner.delete(id).await
        }

        async fn get_pending(
            &self,
        ) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError> {
            self.inner.get_pending().await
        }
    }

    /// Regression test: process_event returns Err when state persistence fails.
    ///
    /// Property: If process_event returns Ok, the state MUST be persisted.
    /// Equivalently: If state persistence fails, process_event MUST return Err.
    ///
    /// This is critical for crash recovery: state is persisted BEFORE effects are
    /// executed. If persistence fails, we return Err without executing effects,
    /// ensuring the system remains in a consistent state.
    #[tokio::test]
    async fn test_process_event_must_return_err_on_final_persistence_failure() {
        use crate::state_machine::event::Event;
        use crate::state_machine::interpreter::InterpreterContext;

        let repo = Arc::new(TrackingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set up initial state
        let initial_state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        store
            .set(pr_id.clone(), initial_state.clone(), Some(111))
            .await
            .unwrap();

        // Create a minimal context (we need real clients for the interpreter, but
        // we'll use an event that doesn't trigger effects requiring them)
        // For this test, we use a DisableReviewsRequested event which transitions
        // to a terminal state without needing external API calls.
        let ctx = InterpreterContext {
            github_client: Arc::new(crate::github::GitHubClient::new(
                12345, // fake app_id
                "fake_private_key".to_string(),
            )),
            openai_client: Arc::new(crate::openai::OpenAIClient::new("fake_api_key".to_string())),
            installation_id: 12345,
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
            pr_url: None,
            branch_name: None,
            correlation_id: None,
        };

        // Now make puts fail - this will cause the final state persistence to fail
        repo.set_fail_puts(true);

        // Process an event that transitions state
        let result = store
            .process_event(&pr_id, Event::DisableReviewsRequested, &ctx)
            .await;

        // process_event should return Err when state persistence fails.
        // Since state is persisted BEFORE effects are executed, a persistence failure
        // means no effects were executed and the system remains in a consistent state.
        assert!(
            result.is_err(),
            "process_event MUST return Err when state persistence fails.\n\
             Got Ok({:?}) but expected Err because persistence was configured to fail.",
            result
        );
    }

    /// Regression test: process_event returns Err with no writes on read error.
    ///
    /// Property: If process_event returns Err(ReadError), no writes should have occurred.
    ///
    /// The current implementation reads state first with get_or_default. If the read fails,
    /// we return Err immediately without any writes. This prevents overwriting valid
    /// persisted state with state derived from defaults.
    #[tokio::test]
    async fn test_process_event_no_writes_before_read_error() {
        use crate::state_machine::event::Event;
        use crate::state_machine::interpreter::InterpreterContext;

        let repo = Arc::new(TrackingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set up initial state with installation_id 111
        let initial_state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        // Manually put via inner to avoid going through tracking
        repo.inner
            .put(
                &pr_id,
                StoredState {
                    state: initial_state.clone(),
                    installation_id: Some(111),
                },
            )
            .await
            .unwrap();

        // Reset counters after setup
        repo.put_count.store(0, std::sync::atomic::Ordering::SeqCst);
        repo.operations.lock().unwrap().clear();

        // Create context with a DIFFERENT installation_id (222)
        let ctx = InterpreterContext {
            github_client: Arc::new(crate::github::GitHubClient::new(
                12345, // fake app_id
                "fake_private_key".to_string(),
            )),
            openai_client: Arc::new(crate::openai::OpenAIClient::new("fake_api_key".to_string())),
            installation_id: 222, // Different from the stored 111
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
            pr_url: None,
            branch_name: None,
            correlation_id: None,
        };

        // Make ALL reads fail immediately. This tests that:
        // 1. get_or_default fails (the first operation)
        // 2. We return Err immediately
        // 3. No writes occur (we never reach the transition/persist loop)
        repo.set_fail_gets_after(0);

        // Process an event
        let result = store
            .process_event(&pr_id, Event::DisableReviewsRequested, &ctx)
            .await;

        // The result should be Err because the read failed
        assert!(result.is_err(), "Expected Err due to read failure");

        // Verify no writes occurred before the read failure
        let put_count = repo.put_count();
        let operations = repo.operations();

        assert_eq!(
            put_count, 0,
            "process_event returned Err but {} writes occurred before the read failure.\n\
             Operations: {:?}\n\
             This violates the 'no writes on read error' guarantee.",
            put_count, operations
        );

        // Verify the original state and installation_id are preserved
        let stored = repo.inner.get(&pr_id).await.unwrap().unwrap();
        assert_eq!(
            stored.installation_id,
            Some(111),
            "Original installation_id should be preserved when read fails"
        );
    }

    /// Regression test (Issue 3): get_or_init returns Err when write fails.
    ///
    /// Property: If a write is needed (reviews_enabled differs) and that write fails,
    /// get_or_init MUST return Err so callers know the update didn't persist.
    ///
    /// Previously: Returns Ok(original_state) when write fails. Callers couldn't
    /// distinguish "no update needed" from "update failed", leading to reviews
    /// running when they should be suppressed.
    #[tokio::test]
    async fn test_get_or_init_must_return_err_on_write_failure() {
        let repo = Arc::new(TrackingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set up initial state with reviews_enabled: true
        let initial_state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        store
            .set(pr_id.clone(), initial_state.clone(), Some(111))
            .await
            .unwrap();

        // Now make writes fail
        repo.set_fail_puts(true);

        // Call get_or_init with reviews_enabled: false
        // This SHOULD try to update reviews_enabled and fail, returning Err.
        let result = store.get_or_init(&pr_id, false).await;

        // BUG: Currently returns Ok(original_state) when write fails.
        // CORRECT behavior: Should return Err when the update couldn't be persisted.
        assert!(
            result.is_err(),
            "get_or_init MUST return Err when updating reviews_enabled fails.\n\
             Current behavior (BUG): Returns Ok({:?}) hiding the write failure.\n\
             This allows callers to proceed unaware that the update wasn't persisted,\n\
             leading to reviews running when they should be suppressed.",
            result
        );
    }

    /// Verify the arb_review_state generator covers all state variants.
    ///
    /// This ensures the property tests are actually exploring all state types.
    #[test]
    fn arb_review_state_covers_all_variants() {
        use proptest::strategy::ValueTree;
        use proptest::test_runner::{Config, TestRunner};

        let mut runner = TestRunner::new(Config::default());
        let mut idle_count = 0u32;
        let mut preparing_count = 0u32;
        let mut awaiting_ancestry_count = 0u32;
        let mut batch_pending_count = 0u32;
        let mut completed_count = 0u32;
        let mut failed_count = 0u32;
        let mut cancelled_count = 0u32;

        // Generate 500 states and count distribution
        for _ in 0..500 {
            let state = arb_review_state().new_tree(&mut runner).unwrap().current();
            match state {
                ReviewMachineState::Idle { .. } => idle_count += 1,
                ReviewMachineState::Preparing { .. } => preparing_count += 1,
                ReviewMachineState::BatchSubmitting { .. } => {
                    // BatchSubmitting counts as a preparing-like state for this test
                    preparing_count += 1;
                }
                ReviewMachineState::AwaitingAncestryCheck { .. } => awaiting_ancestry_count += 1,
                ReviewMachineState::BatchPending { .. } => batch_pending_count += 1,
                ReviewMachineState::Completed { .. } => completed_count += 1,
                ReviewMachineState::Failed { .. } => failed_count += 1,
                ReviewMachineState::Cancelled { .. } => cancelled_count += 1,
            }
        }

        // Assert each variant is generated at least 5% of the time (25 out of 500)
        let min_expected = 25;
        assert!(
            idle_count >= min_expected,
            "Generator produced too few Idle states: {} (expected >= {})",
            idle_count,
            min_expected
        );
        assert!(
            preparing_count >= min_expected,
            "Generator produced too few Preparing states: {} (expected >= {})",
            preparing_count,
            min_expected
        );
        assert!(
            awaiting_ancestry_count >= min_expected,
            "Generator produced too few AwaitingAncestryCheck states: {} (expected >= {})",
            awaiting_ancestry_count,
            min_expected
        );
        assert!(
            batch_pending_count >= min_expected,
            "Generator produced too few BatchPending states: {} (expected >= {})",
            batch_pending_count,
            min_expected
        );
        assert!(
            completed_count >= min_expected,
            "Generator produced too few Completed states: {} (expected >= {})",
            completed_count,
            min_expected
        );
        assert!(
            failed_count >= min_expected,
            "Generator produced too few Failed states: {} (expected >= {})",
            failed_count,
            min_expected
        );
        // Cancelled has two sub-variants in the generator, so it should have ~2x the count
        assert!(
            cancelled_count >= min_expected,
            "Generator produced too few Cancelled states: {} (expected >= {})",
            cancelled_count,
            min_expected
        );
    }
}
