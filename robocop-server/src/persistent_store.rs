//! Persistent state store backed by SQLite.
//!
//! This module wraps the in-memory `StateStore` with SQLite persistence,
//! providing restart safety for the PR state machine.
//!
//! # Restart Safety
//!
//! State is persisted BEFORE executing effects, not after. This ensures that
//! if we crash during an external effect (like submitting a batch to OpenAI),
//! we've already recorded the new state.
//!
//! The sequence for each event is:
//! 1. Compute transition (pure) → get new state and effects
//! 2. Atomically persist new state AND pending effects to SQLite (single transaction)
//! 3. Update in-memory state (only after DB transaction commits)
//! 4. Execute effects (may call external APIs)
//! 5. Process result events from effects
//!
//! If we crash during step 4, on restart we'll have the new state already
//! persisted. Effects are designed to be idempotent (via the two-phase commit
//! in `execute_submit_batch`), so re-executing them is safe.
//!
//! # Memory/DB Consistency
//!
//! All mutation operations (set, remove, process_event) follow a "DB-first" pattern:
//! - Persist to SQLite FIRST
//! - Only update in-memory state AFTER the DB operation succeeds
//!
//! This ensures that memory and DB never diverge. If a DB operation fails,
//! memory remains unchanged and an error is returned. This is critical for
//! restart safety: on restart, the DB state is loaded into memory, so they
//! must always be consistent.
//!
//! ## Preparing State Recovery
//!
//! If the server crashes during `FetchData` while in `Preparing` state:
//! - On startup, `recover_preparing_states()` re-drives `FetchData` for stuck PRs
//! - Manual retry via `/review` command also re-drives `FetchData`
//!
//! ## Batch Submission Idempotency
//!
//! The two-phase commit in `execute_submit_batch` (reserve/submit/confirm)
//! ensures idempotency even if we crash during the OpenAI call.
//!
//! # Single-Instance Design
//!
//! **IMPORTANT**: This store is designed for single-instance deployments only.
//!
//! State is loaded into memory at startup and all reads come from memory. The
//! store does NOT re-read from the database on each operation. This means:
//! - Multiple instances sharing a database will NOT see each other's state changes
//! - A second instance can overwrite the first instance's newer state with stale data
//! - There is no read-through caching, locking, or optimistic concurrency control
//!
//! The batch submission table provides idempotency for single-instance crash
//! recovery: the two-phase commit in `batch_submissions` table prevents duplicate
//! OpenAI batch submissions if the server crashes mid-submission and restarts.
//!
//! For true multi-instance support, you would need to either:
//! 1. Use a shared database with read-through caching and optimistic concurrency
//! 2. Partition PRs across instances (e.g., by PR number modulo instance count)
//! 3. Use a distributed coordination service (e.g., etcd, Redis)

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};
use tracing::info;

use crate::db::SqliteDb;
use crate::state_machine::effect::Effect;
use crate::state_machine::event::Event;
use crate::state_machine::interpreter::{execute_effect, EffectResult, InterpreterContext};
use crate::state_machine::state::{BatchId, ReviewMachineState};
use crate::state_machine::store::{PreparingPrInfo, StateMachinePrId, StateStore};

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

    /// Get the underlying database handle.
    ///
    /// Used by the interpreter for batch submission idempotency.
    pub fn db(&self) -> Arc<SqliteDb> {
        self.db.clone()
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
    ///
    /// Returns an error if the database operation fails. The caller MUST handle
    /// this error appropriately - in particular, `process_event` must NOT execute
    /// effects if persistence fails, to maintain restart safety.
    async fn persist(&self, pr_id: &StateMachinePrId, state: &ReviewMachineState) -> Result<()> {
        // Get the installation_id from memory
        let installation_id = self
            .memory_store
            .get_installation_id(pr_id)
            .await
            .unwrap_or(0);

        let db = self.db.clone();
        let pr_id_for_closure = pr_id.clone();
        let state = state.clone();

        // Spawn blocking task for SQLite operation
        tokio::task::spawn_blocking(move || {
            db.upsert_state(&pr_id_for_closure, &state, installation_id)
        })
        .await
        .context("spawn_blocking panicked while persisting state")?
        .context("Failed to persist state to database")
    }

    /// Delete a state from the database.
    ///
    /// Returns an error if the database operation fails.
    async fn delete_from_db(&self, pr_id: &StateMachinePrId) -> Result<()> {
        let db = self.db.clone();
        let pr_id_for_closure = pr_id.clone();

        tokio::task::spawn_blocking(move || db.delete_state(&pr_id_for_closure))
            .await
            .context("spawn_blocking panicked while deleting state")?
            .map(|_| ()) // Discard the bool return value
            .context("Failed to delete state from database")
    }

    /// Atomically persist state and optionally replace pending effects in a single transaction.
    ///
    /// This ensures crash safety: either both state and effects are persisted,
    /// or neither is. Memory is NOT updated here - the caller must update memory
    /// only after this method returns successfully.
    ///
    /// **Pending effects behavior**:
    /// - If `effects` is non-empty: clears existing pending effects and inserts the new ones.
    /// - If `effects` is empty AND `state_changed` is true: clears existing pending effects
    ///   (old effects are stale because the state has moved forward).
    /// - If `effects` is empty AND `state_changed` is false: leaves existing pending effects
    ///   untouched, preserving failed effects for retry when a no-op event triggers this method.
    async fn persist_state_and_effects(
        &self,
        pr_id: &StateMachinePrId,
        state: &ReviewMachineState,
        installation_id: u64,
        effects: &[Effect],
        state_changed: bool,
    ) -> Result<()> {
        let db = self.db.clone();
        let pr_id = pr_id.clone();
        let state = state.clone();
        let effects = effects.to_vec();

        tokio::task::spawn_blocking(move || {
            db.upsert_state_and_effects(&pr_id, &state, installation_id, &effects, state_changed)
        })
        .await
        .context("spawn_blocking panicked while persisting state and effects")?
        .context("Failed to persist state and effects to database")
    }

    /// Delete a pending effect from the database after successful execution.
    async fn delete_pending_effect(&self, effect_id: i64) -> Result<()> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || db.delete_pending_effect(effect_id))
            .await
            .context("spawn_blocking panicked while deleting effect")?
            .context("Failed to delete pending effect from database")
    }

    /// Load pending effects for a PR from the database.
    ///
    /// Returns (effect_id, effect) tuples that can be used for cleanup after execution.
    pub async fn load_pending_effects(
        &self,
        pr_id: &StateMachinePrId,
    ) -> Result<Vec<(i64, Effect)>> {
        let db = self.db.clone();
        let pr_id = pr_id.clone();

        tokio::task::spawn_blocking(move || db.load_pending_effects(&pr_id))
            .await
            .context("spawn_blocking panicked while loading effects")?
            .context("Failed to load pending effects from database")
    }

    /// Load all PRs with pending effects.
    ///
    /// Used at startup to find PRs that need effect replay.
    pub async fn load_prs_with_pending_effects(&self) -> Result<Vec<StateMachinePrId>> {
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || db.load_prs_with_pending_effects())
            .await
            .context("spawn_blocking panicked while loading PRs with effects")?
            .context("Failed to load PRs with pending effects")
    }

    /// Execute a single effect and clean up from DB on success.
    ///
    /// Returns any result events from the effect.
    async fn execute_effect_with_cleanup(
        &self,
        effect_id: i64,
        effect: Effect,
        ctx: &InterpreterContext,
    ) -> Vec<Event> {
        let result = execute_effect(ctx, effect).await;

        match result {
            EffectResult::Ok(events) => {
                // Delete the effect from DB after successful execution
                if let Err(e) = self.delete_pending_effect(effect_id).await {
                    tracing::error!("Failed to delete executed effect {}: {}", effect_id, e);
                }
                events
            }
            EffectResult::Err(err) => {
                // Don't delete - will be retried on restart
                tracing::error!("Effect execution failed: {}", err);
                Vec::new()
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
    ///
    /// # DB-First Consistency
    ///
    /// This method persists to DB BEFORE updating memory. If the DB operation fails,
    /// memory remains unchanged, ensuring consistency.
    ///
    /// Returns an error if the database operation fails.
    pub async fn get_or_init(
        &self,
        pr_id: &StateMachinePrId,
        reviews_enabled: bool,
    ) -> Result<ReviewMachineState> {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        // Check current state (read-only)
        let current_state = self.memory_store.get(pr_id).await;

        let state = match current_state {
            Some(existing) if existing.reviews_enabled() == reviews_enabled => {
                // State exists and reviews_enabled matches - no update needed
                return Ok(existing);
            }
            Some(existing) => {
                // State exists but reviews_enabled changed - update needed
                existing.with_reviews_enabled(reviews_enabled)
            }
            None => {
                // No state exists - create new one
                ReviewMachineState::Idle { reviews_enabled }
            }
        };

        // Persist to DB FIRST
        self.persist(pr_id, &state).await?;

        // Only update memory AFTER DB succeeds
        self.memory_store.set(pr_id.clone(), state.clone()).await;

        Ok(state)
    }

    /// Get the current state for a PR.
    pub async fn get(&self, pr_id: &StateMachinePrId) -> Option<ReviewMachineState> {
        self.memory_store.get(pr_id).await
    }

    /// Set the state for a PR and persist to database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// persistence are atomic.
    ///
    /// # DB-First Consistency
    ///
    /// This method persists to DB BEFORE updating memory. If the DB operation fails,
    /// memory remains unchanged, ensuring consistency.
    ///
    /// Returns an error if the database operation fails.
    pub async fn set(&self, pr_id: StateMachinePrId, state: ReviewMachineState) -> Result<()> {
        let pr_lock = self.get_or_create_pr_lock(&pr_id).await;
        let _guard = pr_lock.lock().await;

        // Persist to DB FIRST
        self.persist(&pr_id, &state).await?;

        // Only update memory AFTER DB succeeds
        self.memory_store.set(pr_id, state).await;

        Ok(())
    }

    /// Remove the state for a PR and delete from database.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// deletion are atomic.
    ///
    /// # DB-First Consistency
    ///
    /// This method deletes from DB BEFORE updating memory. If the DB operation fails,
    /// memory remains unchanged, ensuring consistency.
    ///
    /// Note: We intentionally do NOT remove the lock entry from `pr_locks` after deletion.
    /// Removing the lock creates a race condition: if another task already cloned the Arc
    /// and is waiting on the mutex, they will proceed after we release. If we then remove
    /// the lock entry, a third task could create a NEW lock, allowing concurrent operations
    /// on the same PR. The memory overhead of keeping lock entries is negligible (one
    /// Arc<Mutex<()>> per PR ever seen).
    ///
    /// Returns an error if the database operation fails.
    pub async fn remove(&self, pr_id: &StateMachinePrId) -> Result<Option<ReviewMachineState>> {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        // Delete from DB FIRST
        self.delete_from_db(pr_id).await?;

        // Only update memory AFTER DB succeeds
        let result = self.memory_store.remove(pr_id).await;

        Ok(result)
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

    /// Get all PR IDs with *active* pending batches.
    ///
    /// Returns PR IDs in `BatchPending` or `AwaitingAncestryCheck` states.
    /// Does NOT include `Cancelled` states with `pending_cancel_batch_id`.
    ///
    /// For batch polling, use `get_pending_batches()` instead.
    pub async fn get_pending_pr_ids(&self) -> Vec<StateMachinePrId> {
        self.memory_store.get_pending_pr_ids().await
    }

    /// Get all pending batches with their PR information.
    pub async fn get_pending_batches(&self) -> Vec<(StateMachinePrId, BatchId, u64)> {
        self.memory_store.get_pending_batches().await
    }

    /// Get all PR IDs that are stuck in `Preparing` state.
    ///
    /// Used for startup recovery: if the server crashed after persisting a PR
    /// in `Preparing` state but before/during `FetchData` execution, we need
    /// to re-drive the effect to avoid the PR being stuck indefinitely.
    pub async fn get_preparing_pr_ids(&self) -> Vec<PreparingPrInfo> {
        self.memory_store.get_preparing_pr_ids().await
    }

    /// Process an event for a PR: transition the state and execute effects.
    ///
    /// This method persists state to the database BEFORE executing effects,
    /// providing crash safety. The sequence for each event is:
    /// 1. Compute transition (pure) → get new state and effects
    /// 2. Partition effects: persistable (UI effects) vs non-persistable
    /// 3. Atomically persist new state and (if any) new persistable effects to SQLite
    /// 4. Update in-memory state (only after DB commit succeeds)
    /// 5. Execute non-persistable effects (they don't need tracking)
    /// 6. Execute persistable effects, deleting each from DB after success
    /// 7. Process result events from effects
    ///
    /// If we crash during step 6, on restart we'll have the new state already
    /// persisted AND the pending effects in the database. The `recover_pending_effects`
    /// function replays any effects that weren't completed.
    ///
    /// **Note on pending effects**:
    /// - When a transition produces new persistable effects, old pending effects are
    ///   cleared and replaced with the new ones.
    /// - When a transition changes the state but produces no persistable effects,
    ///   old pending effects are cleared (they're stale for the new state).
    /// - When a transition is a true no-op (state unchanged, no effects), existing
    ///   pending effects are preserved for retry on restart.
    ///
    /// # Memory/DB Consistency
    ///
    /// State and effects are persisted atomically in a single SQLite transaction
    /// (step 3). Memory is only updated AFTER this transaction commits (step 4).
    /// This ensures memory and DB never diverge: if the transaction fails, we
    /// return an error and memory remains unchanged.
    ///
    /// This method holds a per-PR lock to ensure the memory mutation and DB
    /// persistence are atomic. This prevents concurrent events for the same PR
    /// from interleaving their DB writes, which could leave the database with
    /// stale state after a restart.
    ///
    /// Returns an error if state persistence fails. If an error is returned,
    /// effects were NOT executed, maintaining restart safety: the database
    /// state is unchanged and no external side effects occurred.
    pub async fn process_event(
        &self,
        pr_id: &StateMachinePrId,
        event: Event,
        ctx: &InterpreterContext,
    ) -> Result<ReviewMachineState> {
        let pr_lock = self.get_or_create_pr_lock(pr_id).await;
        let _guard = pr_lock.lock().await;

        let mut current_state = self.memory_store.get_or_default(pr_id).await;

        // Event loop: process initial event and any result events from effects
        let mut events_to_process = vec![event];

        while let Some(event) = events_to_process.pop() {
            // Step 1: Compute transition (pure, no I/O)
            let (new_state, effects) =
                self.memory_store
                    .compute_transition(pr_id, current_state.clone(), event);

            // Step 2: Collect persistable effects (preserving original indices)
            // We need these for DB persistence, but we'll execute ALL effects in original order.
            let persistable_effects: Vec<Effect> = effects
                .iter()
                .filter(|e| e.should_persist())
                .cloned()
                .collect();

            // Detect whether the state changed (used for pending effects cleanup)
            let state_changed = current_state != new_state;

            // Step 3: Atomically persist state AND persistable effects BEFORE executing.
            // This ensures crash safety: either both are persisted, or neither is.
            //
            // CRITICAL: We persist to DB BEFORE updating memory. If the DB operation
            // fails, we return early with an error and memory remains unchanged.
            // This ensures memory and DB never diverge.
            self.persist_state_and_effects(
                pr_id,
                &new_state,
                ctx.installation_id,
                &persistable_effects,
                state_changed,
            )
            .await?;

            // Step 4: Only update memory AFTER DB persistence succeeds.
            // This ensures memory/DB consistency: if we crash after this point,
            // both memory and DB have the new state.
            current_state = new_state;
            self.memory_store
                .set(pr_id.clone(), current_state.clone())
                .await;
            // Also cache installation_id for later use by batch polling.
            // This is set here (after persist) to maintain DB-first consistency.
            self.memory_store
                .set_installation_id(pr_id, ctx.installation_id)
                .await;

            // Step 5: Load effect IDs for persistable effects (for cleanup after execution)
            // We'll use these IDs to clean up from DB after successful execution.
            // The DB returns them in insertion order, matching our persistable_effects order.
            let effect_ids: Vec<i64> = if !persistable_effects.is_empty() {
                match self.load_pending_effects(pr_id).await {
                    Ok(effect_records) => effect_records.into_iter().map(|(id, _)| id).collect(),
                    Err(e) => {
                        tracing::error!(
                            "Failed to load pending effects for PR #{}: {}",
                            pr_id.pr_number,
                            e
                        );
                        // Continue without cleanup tracking
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            // Step 6: Execute ALL effects in their original order.
            // This preserves the transition's declared effect ordering, which may matter
            // if effects have dependencies (e.g., create check run before submitting batch).
            //
            // For persistable effects, we track their position in persistable_effects
            // to look up the corresponding DB ID for cleanup.
            let mut result_events = Vec::new();
            let mut persistable_idx = 0usize;
            for effect in effects {
                let is_persistable = effect.should_persist();
                let events = if is_persistable && persistable_idx < effect_ids.len() {
                    // Persistable effect: execute with cleanup (delete from DB on success)
                    let effect_id = effect_ids[persistable_idx];
                    persistable_idx += 1;
                    self.execute_effect_with_cleanup(effect_id, effect, ctx)
                        .await
                } else {
                    // Non-persistable effect (or cleanup tracking unavailable): just execute
                    if is_persistable {
                        persistable_idx += 1; // Still increment to keep index in sync
                    }
                    match execute_effect(ctx, effect).await {
                        EffectResult::Ok(events) => events,
                        EffectResult::Err(err) => {
                            tracing::error!("Effect execution failed: {}", err);
                            Vec::new()
                        }
                    }
                };
                result_events.extend(events);
            }

            // Step 7: Add result events to be processed
            // (in reverse order so they're processed in order)
            for result_event in result_events.into_iter().rev() {
                events_to_process.push(result_event);
            }
        }

        info!(
            "Final state for PR #{}: {:?}",
            pr_id.pr_number, current_state
        );

        Ok(current_state)
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

        store
            .set(pr_id.clone(), state.clone())
            .await
            .expect("should persist state");

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
            store
                .set(pr_id.clone(), state.clone())
                .await
                .expect("should persist state");

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
            store
                .set(pr_id.clone(), state.clone())
                .await
                .expect("should persist state");
            store.remove(&pr_id).await.expect("should remove state");
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
            store
                .set(pr_id.clone(), state.clone())
                .await
                .expect("should persist state");
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
