//! Startup reconciliation for crash recovery.
//!
//! When the server starts, we check for any PRs in the `BatchSubmitting` state.
//! These represent batch submissions that were in-flight when the previous
//! instance crashed. We query OpenAI to find any matching batches and reconcile.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::{error, info, warn};

use crate::state_machine::event::Event;
use crate::state_machine::interpreter::InterpreterContext;
use crate::state_machine::state::BatchId;
use crate::AppState;

/// Maximum number of batches to list from OpenAI for reconciliation.
/// We paginate through all results to ensure we don't miss any.
const MAX_BATCH_LIST_LIMIT: u32 = 100;

/// Reconcile any PRs stuck in BatchSubmitting state after a crash.
///
/// This function should be called on server startup, after the state store
/// is initialized but before accepting new requests.
///
/// For each PR in BatchSubmitting state:
/// 1. Query OpenAI for batches matching the reconciliation_token
/// 2. If found: emit ReconciliationComplete event to transition to BatchPending
/// 3. If not found: emit ReconciliationFailed event to retry from Preparing
pub async fn reconcile_orphaned_batches(state: Arc<AppState>) {
    info!("Starting crash recovery reconciliation...");

    // Get all PRs in BatchSubmitting state
    let submitting_states = state.state_store.get_submitting_states().await;

    if submitting_states.is_empty() {
        info!("No orphaned batch submissions found. Reconciliation complete.");
        return;
    }

    info!(
        "Found {} PR(s) in BatchSubmitting state, attempting reconciliation...",
        submitting_states.len()
    );

    // Build a map of reconciliation_token -> (pr_id, installation_id)
    let mut token_to_pr: HashMap<String, (crate::state_machine::store::StateMachinePrId, u64)> =
        HashMap::new();
    for (pr_id, token, installation_id) in &submitting_states {
        token_to_pr.insert(token.clone(), (pr_id.clone(), *installation_id));
    }

    // List all batches from OpenAI
    // We need to paginate to get all batches
    let mut all_batches = Vec::new();
    let mut after_cursor: Option<String> = None;

    loop {
        match state
            .openai_client
            .list_batches(None, Some(MAX_BATCH_LIST_LIMIT), after_cursor.as_deref())
            .await
        {
            Ok(response) => {
                all_batches.extend(response.data);

                if response.has_more {
                    after_cursor = response.last_id;
                } else {
                    break;
                }
            }
            Err(e) => {
                error!(
                    "Failed to list batches from OpenAI: {}. Reconciliation aborted.",
                    e
                );
                return;
            }
        }
    }

    info!(
        "Listed {} batches from OpenAI, searching for matches...",
        all_batches.len()
    );

    // Match batches to PRs by reconciliation_token
    let mut matched_tokens: HashMap<String, (String, Option<String>, Option<String>)> =
        HashMap::new(); // token -> (batch_id, model, reasoning_effort)

    for batch in &all_batches {
        if let Some(metadata) = &batch.metadata {
            if let Some(token) = metadata.get("reconciliation_token") {
                if token_to_pr.contains_key(token) {
                    // Found a match!
                    let model = metadata.get("model").cloned();
                    let reasoning_effort = metadata.get("reasoning_effort").cloned();
                    matched_tokens
                        .insert(token.clone(), (batch.id.clone(), model, reasoning_effort));
                }
            }
        }
    }

    // Process each PR in BatchSubmitting state
    for (pr_id, token, installation_id) in submitting_states {
        let event = if let Some((batch_id, model, reasoning_effort)) = matched_tokens.get(&token) {
            info!(
                "Reconciliation: Found batch {} for PR #{} (token: {})",
                batch_id, pr_id.pr_number, token
            );

            Event::ReconciliationComplete {
                batch_id: BatchId::from(batch_id.clone()),
                comment_id: None,   // We don't have this info from OpenAI metadata
                check_run_id: None, // We don't have this info from OpenAI metadata
                model: model.clone().unwrap_or_else(|| "unknown".to_string()),
                reasoning_effort: reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "medium".to_string()),
            }
        } else {
            warn!(
                "Reconciliation: No batch found for PR #{} (token: {}). Will retry submission.",
                pr_id.pr_number, token
            );

            Event::ReconciliationFailed {
                reconciliation_token: token.clone(),
                error: "No matching batch found at OpenAI".to_string(),
            }
        };

        // Create an interpreter context for this PR
        let ctx = InterpreterContext {
            github_client: state.github_client.clone(),
            openai_client: state.openai_client.clone(),
            repo_owner: pr_id.repo_owner.clone(),
            repo_name: pr_id.repo_name.clone(),
            pr_number: pr_id.pr_number,
            pr_url: None,
            branch_name: None,
            installation_id,
            correlation_id: None,
        };

        // Process the reconciliation event
        match state.state_store.process_event(&pr_id, event, &ctx).await {
            Ok(new_state) => {
                info!(
                    "Reconciliation: PR #{} transitioned to {:?}",
                    pr_id.pr_number,
                    std::mem::discriminant(&new_state)
                );
            }
            Err(e) => {
                error!(
                    "Reconciliation: Failed to process event for PR #{}: {}",
                    pr_id.pr_number, e
                );
            }
        }
    }

    info!("Crash recovery reconciliation complete.");
}

#[cfg(test)]
mod tests {
    /// BUG: Reconciliation uses incorrect defaults for model/reasoning_effort.
    ///
    /// When metadata is missing from OpenAI batch, reconciliation defaults to:
    /// - model: "unknown"
    /// - reasoning_effort: "medium"
    ///
    /// But the actual defaults used by the system are:
    /// - model: DEFAULT_MODEL (from robocop_core::DEFAULT_MODEL)
    /// - reasoning_effort: "xhigh" (from preparing.rs DEFAULT_REASONING_EFFORT)
    ///
    /// This causes inconsistent information to be shown in recovered batches.
    #[test]
    fn test_bug_reconciliation_uses_wrong_defaults() {
        // The defaults used in reconciliation.rs
        const RECONCILIATION_DEFAULT_MODEL: &str = "unknown";
        const RECONCILIATION_DEFAULT_REASONING_EFFORT: &str = "medium";

        // The actual defaults used when creating batches
        const ACTUAL_DEFAULT_MODEL: &str = crate::openai::DEFAULT_MODEL;
        const ACTUAL_DEFAULT_REASONING_EFFORT: &str = "xhigh"; // from preparing.rs

        // BUG: These should match, but they don't
        assert_eq!(
            RECONCILIATION_DEFAULT_MODEL, ACTUAL_DEFAULT_MODEL,
            "Reconciliation default model '{}' does not match actual default '{}'.\n\
             This causes inconsistent info on recovered batches.",
            RECONCILIATION_DEFAULT_MODEL, ACTUAL_DEFAULT_MODEL
        );
        assert_eq!(
            RECONCILIATION_DEFAULT_REASONING_EFFORT, ACTUAL_DEFAULT_REASONING_EFFORT,
            "Reconciliation default reasoning_effort '{}' does not match actual default '{}'.\n\
             This causes inconsistent info on recovered batches.",
            RECONCILIATION_DEFAULT_REASONING_EFFORT, ACTUAL_DEFAULT_REASONING_EFFORT
        );
    }
}
