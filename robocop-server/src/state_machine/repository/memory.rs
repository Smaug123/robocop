//! In-memory implementation of `StateRepository`.
//!
//! This provides the same behavior as the original `StateStore` implementation:
//! all state is held in memory and lost on restart.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

use super::{StateRepository, StoredState};
use crate::state_machine::store::StateMachinePrId;

/// In-memory state repository.
///
/// Stores PR states in a `HashMap` protected by a `RwLock`.
/// All state is lost on restart.
pub struct InMemoryRepository {
    states: RwLock<HashMap<StateMachinePrId, StoredState>>,
}

impl InMemoryRepository {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateRepository for InMemoryRepository {
    async fn get(&self, id: &StateMachinePrId) -> Option<StoredState> {
        let states = self.states.read().await;
        states.get(id).cloned()
    }

    async fn put(&self, id: &StateMachinePrId, state: StoredState) {
        let mut states = self.states.write().await;
        states.insert(id.clone(), state);
    }

    async fn delete(&self, id: &StateMachinePrId) -> Option<StoredState> {
        let mut states = self.states.write().await;
        states.remove(id)
    }

    async fn get_pending(&self) -> Vec<(StateMachinePrId, StoredState)> {
        let states = self.states.read().await;
        states
            .iter()
            .filter(|(_, stored)| stored.state.has_pending_batch())
            .map(|(id, stored)| (id.clone(), stored.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::{
        BatchId, CheckRunId, CommentId, CommitSha, ReviewMachineState,
    };

    fn test_pr_id(pr_number: u64) -> StateMachinePrId {
        StateMachinePrId::new("owner", "repo", pr_number)
    }

    fn idle_state(installation_id: u64) -> StoredState {
        StoredState {
            state: ReviewMachineState::Idle {
                reviews_enabled: true,
            },
            installation_id,
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
            installation_id,
        }
    }

    #[tokio::test]
    async fn test_get_returns_none_for_missing() {
        let repo = InMemoryRepository::new();
        let result = repo.get(&test_pr_id(1)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_put_then_get() {
        let repo = InMemoryRepository::new();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state.clone()).await;
        let result = repo.get(&pr_id).await;

        assert!(result.is_some());
        let retrieved = result.unwrap();
        assert_eq!(retrieved.installation_id, 12345);
    }

    #[tokio::test]
    async fn test_delete() {
        let repo = InMemoryRepository::new();
        let pr_id = test_pr_id(1);
        let state = idle_state(12345);

        repo.put(&pr_id, state).await;
        let deleted = repo.delete(&pr_id).await;
        assert!(deleted.is_some());

        let after = repo.get(&pr_id).await;
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn test_get_pending_returns_only_pending() {
        let repo = InMemoryRepository::new();

        // Add an idle state (not pending)
        repo.put(&test_pr_id(1), idle_state(111)).await;

        // Add a pending batch state
        repo.put(&test_pr_id(2), pending_state(222)).await;

        // Add another idle state
        repo.put(&test_pr_id(3), idle_state(333)).await;

        let pending = repo.get_pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.pr_number, 2);
        assert_eq!(pending[0].1.installation_id, 222);
    }
}
