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
use crate::state_machine::state::{BatchId, CheckRunId, CommentId};
use crate::state_machine::transition::{DEFAULT_MODEL, DEFAULT_REASONING_EFFORT};
use crate::AppState;

/// Maximum number of batches to list from OpenAI for reconciliation.
/// We paginate through all results to ensure we don't miss any.
const MAX_BATCH_LIST_LIMIT: u32 = 100;

/// Number of retry attempts for listing batches from OpenAI.
const LIST_BATCHES_MAX_RETRIES: u32 = 3;

/// Delay between retry attempts (in milliseconds).
const LIST_BATCHES_RETRY_DELAY_MS: u64 = 1000;

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

    // List all batches from OpenAI with retry logic
    // We need to paginate to get all batches
    let mut all_batches = Vec::new();
    let mut after_cursor: Option<String> = None;
    let mut retry_count = 0;

    loop {
        match state
            .openai_client
            .list_batches(None, Some(MAX_BATCH_LIST_LIMIT), after_cursor.as_deref())
            .await
        {
            Ok(response) => {
                // Reset retry count on success
                retry_count = 0;
                all_batches.extend(response.data);

                if response.has_more {
                    after_cursor = response.last_id;
                } else {
                    break;
                }
            }
            Err(e) => {
                retry_count += 1;
                if retry_count >= LIST_BATCHES_MAX_RETRIES {
                    error!(
                        "Failed to list batches from OpenAI after {} attempts: {}. Reconciliation aborted.",
                        LIST_BATCHES_MAX_RETRIES, e
                    );
                    return;
                }
                warn!(
                    "Failed to list batches from OpenAI (attempt {}/{}): {}. Retrying in {}ms...",
                    retry_count, LIST_BATCHES_MAX_RETRIES, e, LIST_BATCHES_RETRY_DELAY_MS
                );
                tokio::time::sleep(std::time::Duration::from_millis(LIST_BATCHES_RETRY_DELAY_MS))
                    .await;
            }
        }
    }

    info!(
        "Listed {} batches from OpenAI, searching for matches...",
        all_batches.len()
    );

    // Match batches to PRs by reconciliation_token
    // We store (batch_id, model, reasoning_effort, comment_id, check_run_id)
    #[derive(Default)]
    struct BatchMatch {
        batch_id: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        comment_id: Option<u64>,
        check_run_id: Option<u64>,
    }

    let mut matched_tokens: HashMap<String, BatchMatch> = HashMap::new();

    for batch in &all_batches {
        if let Some(metadata) = &batch.metadata {
            if let Some(token) = metadata.get("reconciliation_token") {
                if token_to_pr.contains_key(token) {
                    // Found a match!
                    matched_tokens.insert(
                        token.clone(),
                        BatchMatch {
                            batch_id: batch.id.clone(),
                            model: metadata.get("model").cloned(),
                            reasoning_effort: metadata.get("reasoning_effort").cloned(),
                            comment_id: metadata.get("comment_id").and_then(|s| s.parse().ok()),
                            check_run_id: metadata.get("check_run_id").and_then(|s| s.parse().ok()),
                        },
                    );
                }
            }
        }
    }

    // Process each PR in BatchSubmitting state
    for (pr_id, token, installation_id) in submitting_states {
        let event = if let Some(batch_match) = matched_tokens.get(&token) {
            info!(
                "Reconciliation: Found batch {} for PR #{} (token: {})",
                batch_match.batch_id, pr_id.pr_number, token
            );

            Event::ReconciliationComplete {
                batch_id: BatchId::from(batch_match.batch_id.clone()),
                comment_id: batch_match.comment_id.map(CommentId),
                check_run_id: batch_match.check_run_id.map(CheckRunId),
                model: batch_match
                    .model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                reasoning_effort: batch_match
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string()),
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
    use super::*;

    /// Test that reconciliation uses the same defaults as batch creation.
    ///
    /// When metadata is missing from OpenAI batch, reconciliation should use
    /// the same defaults that are used when creating new batches.
    #[test]
    fn test_reconciliation_uses_correct_defaults() {
        // The actual defaults used when creating batches
        const ACTUAL_DEFAULT_MODEL: &str = crate::openai::DEFAULT_MODEL;
        const ACTUAL_DEFAULT_REASONING_EFFORT: &str = "xhigh"; // from preparing.rs

        // Reconciliation should use the same defaults
        assert_eq!(
            DEFAULT_MODEL, ACTUAL_DEFAULT_MODEL,
            "Reconciliation DEFAULT_MODEL '{}' should match batch creation default '{}'",
            DEFAULT_MODEL, ACTUAL_DEFAULT_MODEL
        );
        assert_eq!(
            DEFAULT_REASONING_EFFORT, ACTUAL_DEFAULT_REASONING_EFFORT,
            "Reconciliation DEFAULT_REASONING_EFFORT '{}' should match batch creation default '{}'",
            DEFAULT_REASONING_EFFORT, ACTUAL_DEFAULT_REASONING_EFFORT
        );
    }
}
