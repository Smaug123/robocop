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
        }
    }

    /// Get the current state for a PR, or create a default idle state.
    pub async fn get_or_default(&self, pr_id: &StateMachinePrId) -> ReviewMachineState {
        let states = self.states.read().await;
        states.get(pr_id).cloned().unwrap_or_default()
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
        states.remove(pr_id)
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
    /// Returns a list of (pr_id, batch_id) tuples for all PRs with pending batches.
    pub async fn get_pending_batches(&self) -> Vec<(StateMachinePrId, super::state::BatchId)> {
        let states = self.states.read().await;
        states
            .iter()
            .filter_map(|(id, state)| {
                state
                    .pending_batch_id()
                    .map(|batch_id| (id.clone(), batch_id.clone()))
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
        let mut current_state = self.get_or_default(pr_id).await;

        // Event loop: process initial event and any result events from effects
        let mut events_to_process = vec![event];

        while let Some(event) = events_to_process.pop() {
            info!(
                "Processing event {:?} for PR #{} in state {:?}",
                event, pr_id.pr_number, current_state
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
pub fn enable_reviews_event(head_sha: impl Into<String>, base_sha: impl Into<String>) -> Event {
    Event::EnableReviewsRequested {
        head_sha: CommitSha::from(head_sha.into()),
        base_sha: CommitSha::from(base_sha.into()),
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
}
