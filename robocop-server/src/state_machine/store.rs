//! State store for per-PR state machines.
//!
//! This module provides a thread-safe store for managing state machines
//! for each pull request. It integrates with the transition function and
//! effect interpreter to handle state changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use super::event::Event;
use super::interpreter::{execute_effects, InterpreterContext};
use super::state::{CommitSha, ReviewMachineState, ReviewOptions};
use super::transition::{transition, TransitionResult};
use crate::github::GitHubClient;
use crate::openai::OpenAIClient;

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
pub struct StateStore {
    states: RwLock<HashMap<StateMachinePrId, ReviewMachineState>>,
    /// Installation IDs for each PR (needed for batch polling to auth with GitHub).
    installation_ids: RwLock<HashMap<StateMachinePrId, u64>>,
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
        }
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
    pub async fn get_or_init(
        &self,
        pr_id: &StateMachinePrId,
        reviews_enabled: bool,
    ) -> ReviewMachineState {
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
    pub async fn remove(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        let mut states = self.states.write().await;
        let mut installation_ids = self.installation_ids.write().await;
        installation_ids.remove(pr_id);
        states.remove(pr_id)
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

    /// Process an event for a PR: transition the state and execute effects.
    ///
    /// This is the main entry point for handling events. It:
    /// 1. Gets (or creates) the current state
    /// 2. Runs the transition function
    /// 3. Executes effects via the interpreter
    /// 4. Handles result events recursively
    /// 5. Stores the final state
    ///
    /// Returns the final state after all transitions.
    pub async fn process_event(
        &self,
        pr_id: &StateMachinePrId,
        event: Event,
        ctx: &InterpreterContext,
    ) -> ReviewMachineState {
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
            comment_id: CommentId(1),
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
}
