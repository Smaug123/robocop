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
use super::repository::{InMemoryRepository, StateRepository, StoredState};
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
    /// Logs an error and returns the default state if a storage error occurs.
    pub async fn get_or_default(&self, pr_id: &StateMachinePrId) -> ReviewMachineState {
        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => stored.state,
            Ok(None) => ReviewMachineState::default(),
            Err(e) => {
                error!(
                    "Repository error getting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                ReviewMachineState::default()
            }
        }
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

        match self.repository.get(pr_id).await {
            Ok(Some(stored)) => {
                // If reviews_enabled changed (e.g., user edited PR description),
                // update the state to reflect the new value
                if stored.state.reviews_enabled() != reviews_enabled {
                    let updated_state = stored.state.with_reviews_enabled(reviews_enabled);
                    if let Err(e) = self
                        .repository
                        .put(
                            pr_id,
                            StoredState {
                                state: updated_state.clone(),
                                installation_id: stored.installation_id,
                            },
                        )
                        .await
                    {
                        error!(
                            "Repository error updating state for PR #{}: {}",
                            pr_id.pr_number, e
                        );
                    }
                    return updated_state;
                }
                stored.state
            }
            Ok(None) => {
                // Initialize with the given reviews_enabled
                // Note: installation_id is None until process_event is called with a real ID
                let state = ReviewMachineState::Idle { reviews_enabled };
                if let Err(e) = self
                    .repository
                    .put(
                        pr_id,
                        StoredState {
                            state: state.clone(),
                            installation_id: None, // Will be set when process_event is called
                        },
                    )
                    .await
                {
                    error!(
                        "Repository error creating state for PR #{}: {}",
                        pr_id.pr_number, e
                    );
                }
                state
            }
            Err(e) => {
                error!(
                    "Repository error getting state for PR #{}: {}",
                    pr_id.pr_number, e
                );
                // Return a default state on error
                ReviewMachineState::Idle { reviews_enabled }
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
    /// # Safety
    /// This method must only be called while holding the per-PR lock to prevent
    /// read-modify-write races. It is intentionally private; external callers
    /// should use `process_event` which handles locking.
    async fn set(
        &self,
        pr_id: StateMachinePrId,
        state: ReviewMachineState,
        installation_id: Option<u64>,
    ) {
        if let Err(e) = self
            .repository
            .put(
                &pr_id,
                StoredState {
                    state,
                    installation_id,
                },
            )
            .await
        {
            error!(
                "Repository error setting state for PR #{}: {}",
                pr_id.pr_number, e
            );
        }
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

    /// Set the installation ID for a PR.
    ///
    /// This preserves the existing state if one exists. If a repository read
    /// error occurs, the write is skipped to avoid overwriting real state with
    /// a default.
    ///
    /// # Safety
    /// This method must only be called while holding the per-PR lock to prevent
    /// read-modify-write races. It is intentionally private; external callers
    /// should use `process_event` which handles locking.
    async fn set_installation_id(&self, pr_id: &StateMachinePrId, installation_id: u64) {
        // Read directly (not via get_or_default) to distinguish "not found" from "error".
        // On error, we skip the write to avoid overwriting real state with a default.
        let state = match self.repository.get(pr_id).await {
            Ok(Some(stored)) => stored.state,
            Ok(None) => ReviewMachineState::default(),
            Err(e) => {
                error!(
                    "Repository error reading state for PR #{} while setting installation_id: {}; \
                     skipping write to avoid overwriting real state",
                    pr_id.pr_number, e
                );
                return;
            }
        };

        if let Err(e) = self
            .repository
            .put(
                pr_id,
                StoredState {
                    state,
                    installation_id: Some(installation_id),
                },
            )
            .await
        {
            error!(
                "Repository error setting installation_id for PR #{}: {}",
                pr_id.pr_number, e
            );
        }
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
            info!(
                "Processing event {} for PR #{} in state {:?}",
                event.log_summary(),
                pr_id.pr_number,
                current_state
            );

            let TransitionResult { state, effects } = transition(current_state, event);
            current_state = state;

            if !effects.is_empty() {
                info!(
                    "Executing {} effects for PR #{}",
                    effects.len(),
                    pr_id.pr_number
                );

                // Execute effects and collect result events
                let result_events = execute_effects(ctx, effects).await;

                // Add result events to be processed (in reverse order so they're processed in order)
                for result_event in result_events.into_iter().rev() {
                    events_to_process.push(result_event);
                }
            }
        }

        // Store the final state with the installation_id we received.
        // We pass the installation_id explicitly rather than reading it back from
        // the repository, to avoid losing it on transient read errors.
        self.set(
            pr_id.clone(),
            current_state.clone(),
            Some(ctx.installation_id),
        )
        .await;

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
        store.set(pr_id.clone(), state.clone(), Some(12345)).await;

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
        store.set(pr_id.clone(), state.clone(), Some(12345)).await;

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
        store.set(pr_id.clone(), initial_state, Some(12345)).await;

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
                let state = store.get_or_init(&pr_id, reviews_enabled).await;
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
                store.get_or_init(&pr_id, true).await
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
                return Err(RepositoryError::StorageError(
                    "simulated read failure".to_string(),
                ));
            }
            self.inner.get(id).await
        }

        async fn put(
            &self,
            id: &StateMachinePrId,
            state: StoredState,
        ) -> Result<(), RepositoryError> {
            if self.fail_puts.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(RepositoryError::StorageError(
                    "simulated write failure".to_string(),
                ));
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

    /// Test that set_installation_id skips writes when repository read fails.
    ///
    /// This prevents overwriting real state with a default on transient errors.
    /// Regression test for: repository read errors treated as "missing".
    #[tokio::test]
    async fn test_set_installation_id_skips_write_on_read_error() {
        let repo = Arc::new(FailingRepository::new());
        let store = StateStore::with_repository(repo.clone());
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // First, write a state with installation_id 111
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        store.set(pr_id.clone(), state.clone(), Some(111)).await;

        // Verify the state was written correctly
        let stored = repo.inner.get(&pr_id).await.unwrap().unwrap();
        assert_eq!(stored.installation_id, Some(111));
        assert!(!stored.state.reviews_enabled());

        // Now make reads fail
        repo.set_fail_gets(true);

        // Try to set a new installation_id - this should skip the write
        store.set_installation_id(&pr_id, 222).await;

        // Stop failing
        repo.set_fail_gets(false);

        // Verify the original state is preserved (not overwritten with default)
        let stored = repo.inner.get(&pr_id).await.unwrap().unwrap();
        assert_eq!(
            stored.installation_id,
            Some(111),
            "Original installation_id should be preserved when read fails"
        );
        assert!(
            !stored.state.reviews_enabled(),
            "Original state should be preserved when read fails"
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
        store.set(pr_id.clone(), initial_state, Some(12345)).await;

        // Verify installation_id is set
        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(installation_id, Some(12345));

        // Update to a different state, passing the same installation_id
        let new_state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        store.set(pr_id.clone(), new_state, Some(12345)).await;

        // Verify installation_id is still present
        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(
            installation_id,
            Some(12345),
            "installation_id should be preserved across state updates"
        );
    }

    /// Test that set_installation_id correctly updates the installation_id
    /// while preserving the existing state.
    #[tokio::test]
    async fn test_set_installation_id_preserves_state() {
        use crate::state_machine::state::{BatchId, CheckRunId, CommentId, CommitSha};

        let store = StateStore::new();
        let pr_id = StateMachinePrId::new("owner", "repo", 123);

        // Set up a BatchPending state with installation_id 111
        let pending_state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_xyz".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(42)),
            check_run_id: Some(CheckRunId(99)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        store
            .set(pr_id.clone(), pending_state.clone(), Some(111))
            .await;

        // Update installation_id to 222
        store.set_installation_id(&pr_id, 222).await;

        // Verify state is preserved but installation_id is updated
        let state = store.get(&pr_id).await.unwrap();
        assert!(
            matches!(state, ReviewMachineState::BatchPending { batch_id, .. } if batch_id.0 == "batch_xyz"),
            "State should be preserved after set_installation_id"
        );

        let installation_id = store.get_installation_id(&pr_id).await;
        assert_eq!(
            installation_id,
            Some(222),
            "installation_id should be updated"
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
        store.set(pr_id.clone(), state, Some(12345)).await;

        // Call get_or_init with different reviews_enabled value
        // This should update reviews_enabled but preserve installation_id
        let result = store.get_or_init(&pr_id, false).await;

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
        /// Property: set_installation_id preserves the state exactly, only changing installation_id.
        ///
        /// For any state, calling set_installation_id should not modify the state itself.
        #[test]
        fn set_installation_id_preserves_state_for_all_variants(
            state in arb_review_state(),
            initial_id in 1u64..1000000,
            new_id in 1u64..1000000
        ) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async {
                let store = StateStore::new();
                let pr_id = test_pr_id(1);

                // Set initial state with initial_id
                store.set(pr_id.clone(), state.clone(), Some(initial_id)).await;

                // Update only the installation_id
                store.set_installation_id(&pr_id, new_id).await;

                // Verify state is unchanged
                let retrieved = store.get(&pr_id).await.expect("state should exist");
                assert_eq!(
                    retrieved, state,
                    "State should be preserved after set_installation_id.\n\
                     Expected: {:?}\n\
                     Got: {:?}",
                    state, retrieved
                );

                // Verify installation_id is updated
                let retrieved_id = store.get_installation_id(&pr_id).await;
                assert_eq!(
                    retrieved_id, Some(new_id),
                    "installation_id should be updated to new value"
                );
            });
        }

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
                store.set(pr_id.clone(), state.clone(), Some(installation_id)).await;

                // Call get_or_init with potentially different reviews_enabled
                let _ = store.get_or_init(&pr_id, new_reviews_enabled).await;

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
                store.set(pr_id.clone(), state.clone(), Some(installation_id)).await;

                // Call get_or_init
                let result = store.get_or_init(&pr_id, requested_reviews_enabled).await;

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
