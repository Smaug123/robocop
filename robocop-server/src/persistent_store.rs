//! Persistent state store backed by SQLite.
//!
//! This module wraps the in-memory `StateStore` with SQLite persistence,
//! providing restart safety for the PR state machine.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info};

use crate::db::SqliteDb;
use crate::state_machine::event::Event;
use crate::state_machine::interpreter::InterpreterContext;
use crate::state_machine::state::{BatchId, ReviewMachineState};
use crate::state_machine::store::{StateMachinePrId, StateStore};

/// Persistent state store that wraps `StateStore` with SQLite persistence.
///
/// Uses write-through caching: the in-memory `StateStore` is the fast path,
/// and changes are persisted to SQLite after each mutation.
///
/// # Concurrency
///
/// This store maintains per-PR locks to ensure that memory mutations and their
/// corresponding DB writes are atomic. Without this, concurrent events for the
/// same PR could interleave such that DB writes complete out-of-order, leaving
/// the database with stale state after a restart.
pub struct PersistentStateStore {
    memory_store: StateStore,
    db: Arc<SqliteDb>,
    /// Per-PR locks to serialize memory mutations and DB persistence together.
    ///
    /// This ensures that for any given PR, the sequence "mutate memory → persist to DB"
    /// is atomic with respect to other operations on the same PR.
    pr_locks: RwLock<HashMap<StateMachinePrId, Arc<Mutex<()>>>>,
}

impl PersistentStateStore {
    /// Create a new persistent state store.
    ///
    /// Opens or creates the SQLite database at the given path, loads all
    /// persisted states into memory, and returns the store ready for use.
    pub async fn new(db_path: &Path) -> Result<Self> {
        // Create the database (this is a blocking operation)
        let path = db_path.to_path_buf();
        let db = tokio::task::spawn_blocking(move || SqliteDb::new(&path))
            .await
            .context("spawn_blocking panicked")?
            .context("Failed to open SQLite database")?;

        let db = Arc::new(db);
        let memory_store = StateStore::new();

        // Load all persisted states
        let db_clone = db.clone();
        let states = tokio::task::spawn_blocking(move || db_clone.load_all_states())
            .await
            .context("spawn_blocking panicked")?
            .context("Failed to load states from database")?;

        info!("Loaded {} persisted PR states from database", states.len());

        // Populate the memory store
        for (pr_id, state, installation_id) in states {
            memory_store.set(pr_id.clone(), state).await;
            memory_store
                .set_installation_id(&pr_id, installation_id)
                .await;
        }

        Ok(Self {
            memory_store,
            db,
            pr_locks: RwLock::new(HashMap::new()),
        })
    }

    /// Create a persistent store with an in-memory database (for testing).
    pub fn new_in_memory() -> Result<Self> {
        let db = Arc::new(SqliteDb::new_in_memory()?);
        let memory_store = StateStore::new();
        Ok(Self {
            memory_store,
            db,
            pr_locks: RwLock::new(HashMap::new()),
        })
    }

    /// Get or create a lock for a specific PR.
    ///
    /// This lock serializes memory mutations and DB persistence for a PR,
    /// ensuring they complete atomically with respect to other operations.
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

    /// Persist a state to the database.
    async fn persist(&self, pr_id: &StateMachinePrId, state: &ReviewMachineState) {
        // Get the installation_id from memory
        let installation_id = self
            .memory_store
            .get_installation_id(pr_id)
            .await
            .unwrap_or(0);

        let db = self.db.clone();
        let pr_id_for_closure = pr_id.clone();
        let pr_id_for_error = pr_id.clone();
        let state = state.clone();

        // Spawn blocking task for SQLite operation
        let result = tokio::task::spawn_blocking(move || {
            db.upsert_state(&pr_id_for_closure, &state, installation_id)
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(
                    "Failed to persist state for PR {:?}: {}",
                    pr_id_for_error, e
                );
            }
            Err(e) => {
                error!("spawn_blocking panicked while persisting state: {}", e);
            }
        }
    }

    /// Delete a state from the database.
    async fn delete_from_db(&self, pr_id: &StateMachinePrId) {
        let db = self.db.clone();
        let pr_id_for_closure = pr_id.clone();
        let pr_id_for_error = pr_id.clone();

        let result = tokio::task::spawn_blocking(move || db.delete_state(&pr_id_for_closure)).await;

        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                error!("Failed to delete state for PR {:?}: {}", pr_id_for_error, e);
            }
            Err(e) => {
                error!("spawn_blocking panicked while deleting state: {}", e);
            }
        }
    }

    // =========================================================================
    // Delegated methods from StateStore
    // =========================================================================

    /// Get the current state for a PR, or create a default idle state.
    pub async fn get_or_default(&self, pr_id: &StateMachinePrId) -> ReviewMachineState {
        self.memory_store.get_or_default(pr_id).await
    }

    /// Get or initialize the state for a PR with the given reviews_enabled setting.
    ///
    /// If the state is created or modified, it will be persisted to the database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// persistence are atomic.
    pub async fn get_or_init(
        &self,
        pr_id: &StateMachinePrId,
        reviews_enabled: bool,
    ) -> ReviewMachineState {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let state = self.memory_store.get_or_init(pr_id, reviews_enabled).await;
        self.persist(pr_id, &state).await;
        state
    }

    /// Get the current state for a PR.
    pub async fn get(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        self.memory_store.get(pr_id).await
    }

    /// Set the state for a PR and persist to database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// persistence are atomic.
    pub async fn set(&self, pr_id: StateMachinePrId, state: ReviewMachineState) {
        let pr_lock = self.get_or_create_pr_lock(&pr_id).await;
        let _guard = pr_lock.lock().await;

        self.memory_store.set(pr_id.clone(), state.clone()).await;
        self.persist(&pr_id, &state).await;
    }

    /// Remove the state for a PR and delete from database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// deletion are atomic.
    pub async fn remove(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let result = self.memory_store.remove(pr_id).await;
        self.delete_from_db(pr_id).await;

        // Clean up the lock entry after deletion (we're still holding it, so this is safe)
        drop(_guard);
        let mut locks = self.pr_locks.write().await;
        locks.remove(pr_id);

        result
    }

    /// Set the installation ID for a PR.
    ///
    /// Note: This is stored in memory. The installation_id is persisted
    /// alongside the state when the state is persisted.
    pub async fn set_installation_id(&self, pr_id: &StateMachinePrId, installation_id: u64) {
        self.memory_store
            .set_installation_id(pr_id, installation_id)
            .await;
    }

    /// Get the installation ID for a PR.
    pub async fn get_installation_id(&self, pr_id: &StateMachinePrId) -> Option<u64> {
        self.memory_store.get_installation_id(pr_id).await
    }

    /// Get all PR IDs with pending batches.
    pub async fn get_pending_pr_ids(&self) -> Vec<StateMachinePrId> {
        self.memory_store.get_pending_pr_ids().await
    }

    /// Get all pending batches with their PR information.
    pub async fn get_pending_batches(&self) -> Vec<(StateMachinePrId, BatchId, u64)> {
        self.memory_store.get_pending_batches().await
    }

    /// Process an event for a PR: transition the state and execute effects.
    ///
    /// After processing, the final state is persisted to the database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// persistence are atomic. This prevents concurrent events for the same PR
    /// from interleaving their DB writes, which could leave the database with
    /// stale state after a restart.
    pub async fn process_event(
        &self,
        pr_id: &StateMachinePrId,
        event: Event,
        ctx: &InterpreterContext,
    ) -> ReviewMachineState {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let final_state = self.memory_store.process_event(pr_id, event, ctx).await;
        self.persist(pr_id, &final_state).await;
        final_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::{BatchId, CommitSha};

    #[tokio::test]
    async fn test_new_in_memory() {
        let store = PersistentStateStore::new_in_memory().expect("should create store");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = store.get(&pr_id).await;
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let store = PersistentStateStore::new_in_memory().expect("should create store");

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };

        store.set(pr_id.clone(), state.clone()).await;

        let retrieved = store.get(&pr_id).await;
        assert_eq!(retrieved, Some(state));
    }

    #[tokio::test]
    async fn test_persistence_survives_reload() {
        // Create a temporary file for the database
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_robocop_{}.db", std::process::id()));

        // Clean up any existing file
        let _ = std::fs::remove_file(&db_path);

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_abc".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        // Create store, insert state
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");
            store.set_installation_id(&pr_id, 12345).await;
            store.set(pr_id.clone(), state.clone()).await;

            // Verify it's in memory
            let retrieved = store.get(&pr_id).await;
            assert_eq!(retrieved, Some(state.clone()));
        }

        // Create a new store from the same database - should load persisted state
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");

            let retrieved = store.get(&pr_id).await;
            assert_eq!(retrieved, Some(state));

            let installation_id = store.get_installation_id(&pr_id).await;
            assert_eq!(installation_id, Some(12345));
        }

        // Clean up
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn test_remove_deletes_from_db() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_robocop_remove_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };

        // Insert and remove
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");
            store.set(pr_id.clone(), state.clone()).await;
            store.remove(&pr_id).await;
        }

        // Reload - should be empty
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");
            let retrieved = store.get(&pr_id).await;
            assert!(retrieved.is_none());
        }

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn test_get_pending_batches_after_reload() {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("test_robocop_pending_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        let pr_id = StateMachinePrId::new("owner", "repo", 42);
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_xyz".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        // Insert pending batch
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");
            store.set_installation_id(&pr_id, 12345).await;
            store.set(pr_id.clone(), state.clone()).await;
        }

        // Reload and check pending batches
        {
            let store = PersistentStateStore::new(&db_path)
                .await
                .expect("should create store");

            let pending = store.get_pending_batches().await;
            assert_eq!(pending.len(), 1);

            let (loaded_pr_id, loaded_batch_id, loaded_installation_id) = &pending[0];
            assert_eq!(loaded_pr_id, &pr_id);
            assert_eq!(loaded_batch_id.0, "batch_xyz");
            assert_eq!(*loaded_installation_id, 12345);
        }

        let _ = std::fs::remove_file(&db_path);
    }
}
