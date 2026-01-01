//! Repository abstraction for state machine persistence.
//!
//! This module defines the `StateRepository` trait that abstracts
//! storage operations for PR review states. Implementations can
//! provide different backends (in-memory, SQLite, etc.).

mod memory;

pub use memory::InMemoryRepository;

use async_trait::async_trait;

use super::state::ReviewMachineState;
use super::store::StateMachinePrId;

/// Combined state record for persistence.
///
/// This bundles the state machine state with the installation ID,
/// which is needed for GitHub API authentication during batch polling.
#[derive(Debug, Clone)]
pub struct StoredState {
    pub state: ReviewMachineState,
    pub installation_id: u64,
}

/// Repository trait for persisting PR review states.
///
/// Implementations of this trait provide the actual storage backend.
/// The `StateStore` uses this trait to abstract away storage details,
/// enabling future persistence backends (e.g., SQLite) without changing
/// the state machine coordination logic.
#[async_trait]
pub trait StateRepository: Send + Sync {
    /// Get state for a PR, returning None if not found.
    async fn get(&self, id: &StateMachinePrId) -> Option<StoredState>;

    /// Store state for a PR (upsert semantics).
    async fn put(&self, id: &StateMachinePrId, state: StoredState);

    /// Delete state for a PR.
    async fn delete(&self, id: &StateMachinePrId) -> Option<StoredState>;

    /// Get all states with pending batches.
    ///
    /// Returns states where `ReviewMachineState::pending_batch_id()` is `Some`.
    /// This includes:
    /// - `BatchPending` and `AwaitingAncestryCheck` (active batches)
    /// - `Cancelled` with `pending_cancel_batch_id` (cancel-failed batches that may complete)
    ///
    /// Used by the polling loop to discover batches that need status checks.
    async fn get_pending(&self) -> Vec<(StateMachinePrId, StoredState)>;
}
