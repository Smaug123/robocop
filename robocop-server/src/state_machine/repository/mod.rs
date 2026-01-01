//! Repository abstraction for state machine persistence.
//!
//! This module defines the `StateRepository` trait that abstracts
//! storage operations for PR review states. Implementations can
//! provide different backends (in-memory, SQLite, etc.).

mod memory;

pub use memory::InMemoryRepository;

use async_trait::async_trait;
use std::fmt;

use super::state::ReviewMachineState;
use super::store::StateMachinePrId;

/// Error type for repository operations.
///
/// This allows callers to distinguish between "not found" (None) and
/// "storage error" (Err), which is critical for crash-recovery.
#[derive(Debug, Clone)]
pub enum RepositoryError {
    /// Storage backend is unavailable or failed.
    StorageError(String),
    /// Data is corrupted or invalid.
    DataCorruption(String),
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepositoryError::StorageError(msg) => write!(f, "storage error: {}", msg),
            RepositoryError::DataCorruption(msg) => write!(f, "data corruption: {}", msg),
        }
    }
}

impl std::error::Error for RepositoryError {}

/// Combined state record for persistence.
///
/// This bundles the state machine state with the installation ID,
/// which is needed for GitHub API authentication during batch polling.
#[derive(Debug, Clone)]
pub struct StoredState {
    pub state: ReviewMachineState,
    /// Installation ID for GitHub API authentication.
    /// This is `None` if the state was created before a full event was processed
    /// (e.g., via `get_or_init`). States with `None` installation_id should be
    /// filtered out from batch polling since we can't authenticate to GitHub.
    pub installation_id: Option<u64>,
}

/// Repository trait for persisting PR review states.
///
/// Implementations of this trait provide the actual storage backend.
/// The `StateStore` uses this trait to abstract away storage details,
/// enabling future persistence backends (e.g., SQLite) without changing
/// the state machine coordination logic.
///
/// All methods return `Result` to distinguish between:
/// - Success with data (`Ok(Some(state))` or `Ok(())`)
/// - Key not found (`Ok(None)`)
/// - Storage/IO error (`Err(RepositoryError)`)
///
/// This is critical for crash-recovery: callers can decide whether to
/// use a default value (for "not found") or alert/retry (for errors).
#[async_trait]
pub trait StateRepository: Send + Sync {
    /// Get state for a PR.
    ///
    /// Returns:
    /// - `Ok(Some(state))` if found
    /// - `Ok(None)` if not found
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError>;

    /// Store state for a PR (upsert semantics).
    ///
    /// Returns:
    /// - `Ok(())` on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn put(&self, id: &StateMachinePrId, state: StoredState) -> Result<(), RepositoryError>;

    /// Delete state for a PR.
    ///
    /// Returns:
    /// - `Ok(Some(state))` if state was deleted
    /// - `Ok(None)` if state didn't exist
    /// - `Err(RepositoryError)` if storage operation failed
    async fn delete(&self, id: &StateMachinePrId) -> Result<Option<StoredState>, RepositoryError>;

    /// Get all states with pending batches.
    ///
    /// Returns states where `ReviewMachineState::pending_batch_id()` is `Some`.
    /// This includes:
    /// - `BatchPending` and `AwaitingAncestryCheck` (active batches)
    /// - `Cancelled` with `pending_cancel_batch_id` (cancel-failed batches that may complete)
    ///
    /// Used by the polling loop to discover batches that need status checks.
    ///
    /// Returns:
    /// - `Ok(vec)` with pending states on success
    /// - `Err(RepositoryError)` if storage operation failed
    async fn get_pending(&self) -> Result<Vec<(StateMachinePrId, StoredState)>, RepositoryError>;
}
