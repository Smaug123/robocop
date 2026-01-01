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
/// Higher value reduces false-positive failures during transient API outages.
const LIST_BATCHES_MAX_RETRIES: u32 = 10;

/// Delay between retry attempts (in milliseconds).
const LIST_BATCHES_RETRY_DELAY_MS: u64 = 2000;

/// Number of retry attempts when a batch is not found (eventual consistency).
/// This handles the case where a batch was just submitted but isn't visible yet.
const BATCH_NOT_FOUND_MAX_RETRIES: u32 = 2;

/// Delay between retries when batch not found (in milliseconds).
/// Longer than API error retries since we're waiting for eventual consistency.
const BATCH_NOT_FOUND_RETRY_DELAY_MS: u64 = 5000;

/// Matched batch information extracted from OpenAI batch metadata.
#[derive(Default)]
struct BatchMatch {
    batch_id: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    comment_id: Option<u64>,
    check_run_id: Option<u64>,
    /// Branch name from batch metadata (key: "branch")
    branch_name: Option<String>,
    /// Pull request URL from batch metadata (key: "pull_request_url")
    pr_url: Option<String>,
}

/// Returns true if the batch status indicates it's still in-flight (not terminal).
fn is_in_flight_status(status: &str) -> bool {
    matches!(status, "validating" | "in_progress" | "finalizing")
}

/// Compare two batches and return true if `new` should replace `existing`.
/// Prefers in-flight batches over terminal, and newer batches within the same category.
fn should_replace_batch(
    existing_status: &str,
    existing_created_at: u64,
    new_status: &str,
    new_created_at: u64,
) -> bool {
    let existing_in_flight = is_in_flight_status(existing_status);
    let new_in_flight = is_in_flight_status(new_status);

    match (existing_in_flight, new_in_flight) {
        // New is in-flight, existing is terminal -> replace
        (false, true) => true,
        // Both in same category -> prefer newer
        (true, true) | (false, false) => new_created_at > existing_created_at,
        // Existing is in-flight, new is terminal -> keep existing
        (true, false) => false,
    }
}

/// Find the best matching batch for a token from a list of batches.
/// Returns None if no batch matches the token.
/// When multiple batches match, prefers in-flight over terminal, and newer over older.
fn find_best_batch_for_token(
    batches: &[robocop_core::openai::BatchResponse],
    token: &str,
) -> Option<BatchMatch> {
    let mut best: Option<(BatchMatch, String, u64)> = None; // (match, status, created_at)

    for batch in batches {
        if let Some(metadata) = &batch.metadata {
            if metadata.get("reconciliation_token").map(|t| t.as_str()) == Some(token) {
                let dominated = match &best {
                    None => false,
                    Some((_, existing_status, existing_created_at)) => !should_replace_batch(
                        existing_status,
                        *existing_created_at,
                        &batch.status,
                        batch.created_at,
                    ),
                };

                if !dominated {
                    let branch_name = metadata.get("branch").and_then(|b| {
                        if b == "<no branch>" {
                            None
                        } else {
                            Some(b.clone())
                        }
                    });

                    best = Some((
                        BatchMatch {
                            batch_id: batch.id.clone(),
                            model: metadata.get("model").cloned(),
                            reasoning_effort: metadata.get("reasoning_effort").cloned(),
                            comment_id: metadata.get("comment_id").and_then(|s| s.parse().ok()),
                            check_run_id: metadata.get("check_run_id").and_then(|s| s.parse().ok()),
                            branch_name,
                            pr_url: metadata.get("pull_request_url").cloned(),
                        },
                        batch.status.clone(),
                        batch.created_at,
                    ));
                }
            }
        }
    }

    best.map(|(m, _, _)| m)
}

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
                    match response.last_id {
                        Some(cursor) => {
                            after_cursor = Some(cursor);
                        }
                        None => {
                            // Defensive: If has_more=true but no cursor is provided,
                            // break to avoid infinite loop. This shouldn't happen with
                            // a well-behaved API, but protect against it anyway.
                            warn!(
                                "OpenAI returned has_more=true but no last_id cursor. \
                                 Breaking pagination loop to avoid infinite loop. \
                                 Fetched {} batches so far.",
                                all_batches.len()
                            );
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
            Err(e) => {
                retry_count += 1;
                if retry_count >= LIST_BATCHES_MAX_RETRIES {
                    error!(
                        "Failed to list batches from OpenAI after {} attempts: {}. \
                         Emitting ReconciliationFailed events for all {} BatchSubmitting PRs.",
                        LIST_BATCHES_MAX_RETRIES,
                        e,
                        submitting_states.len()
                    );

                    // Instead of aborting silently, emit ReconciliationFailed for all PRs.
                    // This allows them to retry the batch submission from the Preparing state.
                    for (pr_id, token, installation_id) in &submitting_states {
                        let fallback_pr_url = format!(
                            "https://github.com/{}/{}/pull/{}",
                            pr_id.repo_owner, pr_id.repo_name, pr_id.pr_number
                        );

                        let event = Event::ReconciliationFailed {
                            reconciliation_token: token.clone(),
                            error: format!(
                                "OpenAI API unavailable during reconciliation after {} attempts: {}",
                                LIST_BATCHES_MAX_RETRIES, e
                            ),
                        };

                        let ctx = InterpreterContext {
                            github_client: state.github_client.clone(),
                            openai_client: state.openai_client.clone(),
                            repo_owner: pr_id.repo_owner.clone(),
                            repo_name: pr_id.repo_name.clone(),
                            pr_number: pr_id.pr_number,
                            pr_url: Some(fallback_pr_url),
                            branch_name: None, // Unknown when list_batches fails
                            installation_id: *installation_id,
                            correlation_id: None,
                        };

                        match state.state_store.process_event(pr_id, event, &ctx).await {
                            Ok(new_state) => {
                                warn!(
                                    "Reconciliation (OpenAI unavailable): PR #{} transitioned to {:?}. \
                                     Will retry batch submission.",
                                    pr_id.pr_number,
                                    std::mem::discriminant(&new_state)
                                );
                            }
                            Err(e) => {
                                error!(
                                    "Reconciliation: Failed to process ReconciliationFailed event for PR #{}: {}",
                                    pr_id.pr_number, e
                                );
                            }
                        }
                    }

                    info!(
                        "Crash recovery reconciliation complete (with OpenAI unavailable - PRs will retry)."
                    );
                    return;
                }
                warn!(
                    "Failed to list batches from OpenAI (attempt {}/{}): {}. Retrying in {}ms...",
                    retry_count, LIST_BATCHES_MAX_RETRIES, e, LIST_BATCHES_RETRY_DELAY_MS
                );
                tokio::time::sleep(std::time::Duration::from_millis(
                    LIST_BATCHES_RETRY_DELAY_MS,
                ))
                .await;
            }
        }
    }

    info!(
        "Listed {} batches from OpenAI, searching for matches...",
        all_batches.len()
    );

    // Match batches to PRs by reconciliation_token
    // For each token, find the best matching batch (preferring in-flight over terminal,
    // and newer over older within the same category)
    let mut matched_tokens: HashMap<String, BatchMatch> = HashMap::new();

    for token in token_to_pr.keys() {
        if let Some(batch_match) = find_best_batch_for_token(&all_batches, token) {
            matched_tokens.insert(token.clone(), batch_match);
        }
    }

    // Process each PR in BatchSubmitting state
    for (pr_id, token, installation_id) in submitting_states {
        // Construct fallback PR URL from repo info (always available)
        let fallback_pr_url = format!(
            "https://github.com/{}/{}/pull/{}",
            pr_id.repo_owner, pr_id.repo_name, pr_id.pr_number
        );

        let (event, pr_url, branch_name) = if let Some(batch_match) = matched_tokens.get(&token) {
            info!(
                "Reconciliation: Found batch {} for PR #{} (token: {})",
                batch_match.batch_id, pr_id.pr_number, token
            );

            let event = Event::ReconciliationComplete {
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
            };

            // Use pr_url from batch metadata if available, otherwise construct it
            let pr_url = batch_match.pr_url.clone().unwrap_or(fallback_pr_url);

            (event, Some(pr_url), batch_match.branch_name.clone())
        } else {
            // Batch not found in initial list. This could be due to eventual consistency
            // (batch was just submitted but not visible yet). Retry a few times before giving up.
            let mut found_match: Option<BatchMatch> = None;

            for retry in 1..=BATCH_NOT_FOUND_MAX_RETRIES {
                info!(
                    "Reconciliation: No batch found for PR #{} (token: {}). \
                     Waiting {}ms before retry {}/{}...",
                    pr_id.pr_number,
                    token,
                    BATCH_NOT_FOUND_RETRY_DELAY_MS,
                    retry,
                    BATCH_NOT_FOUND_MAX_RETRIES
                );

                tokio::time::sleep(std::time::Duration::from_millis(
                    BATCH_NOT_FOUND_RETRY_DELAY_MS,
                ))
                .await;

                // Re-list batches from OpenAI
                let mut retry_batches = Vec::new();
                let mut after_cursor: Option<String> = None;
                let mut api_retry_count = 0;

                'pagination: loop {
                    match state
                        .openai_client
                        .list_batches(None, Some(MAX_BATCH_LIST_LIMIT), after_cursor.as_deref())
                        .await
                    {
                        Ok(response) => {
                            api_retry_count = 0;
                            retry_batches.extend(response.data);

                            if response.has_more {
                                if let Some(cursor) = response.last_id {
                                    after_cursor = Some(cursor);
                                } else {
                                    break 'pagination;
                                }
                            } else {
                                break 'pagination;
                            }
                        }
                        Err(e) => {
                            api_retry_count += 1;
                            if api_retry_count >= LIST_BATCHES_MAX_RETRIES {
                                warn!(
                                    "Failed to re-list batches during retry: {}. Continuing with stale data.",
                                    e
                                );
                                break 'pagination;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(
                                LIST_BATCHES_RETRY_DELAY_MS,
                            ))
                            .await;
                        }
                    }
                }

                // Search for our token in the fresh batch list
                if let Some(batch_match) = find_best_batch_for_token(&retry_batches, &token) {
                    info!(
                        "Reconciliation: Found batch {} for PR #{} on retry {}",
                        batch_match.batch_id, pr_id.pr_number, retry
                    );
                    found_match = Some(batch_match);
                }

                if found_match.is_some() {
                    break;
                }
            }

            if let Some(batch_match) = found_match {
                // Found the batch on retry
                let event = Event::ReconciliationComplete {
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
                };

                let pr_url = batch_match.pr_url.clone().unwrap_or(fallback_pr_url);
                (event, Some(pr_url), batch_match.branch_name.clone())
            } else {
                // Still not found after retries - fetch PR details from GitHub for branch name
                warn!(
                    "Reconciliation: No batch found for PR #{} (token: {}) after {} retries. Will retry submission.",
                    pr_id.pr_number, token, BATCH_NOT_FOUND_MAX_RETRIES
                );

                // Try to fetch the current branch name from GitHub
                let branch_name = match state
                    .github_client
                    .get_pull_request(
                        None,
                        installation_id,
                        &pr_id.repo_owner,
                        &pr_id.repo_name,
                        pr_id.pr_number,
                    )
                    .await
                {
                    Ok(pr_response) => {
                        info!(
                            "Reconciliation: Fetched branch name '{}' from GitHub for PR #{}",
                            pr_response.head.ref_name, pr_id.pr_number
                        );
                        Some(pr_response.head.ref_name)
                    }
                    Err(e) => {
                        warn!(
                            "Reconciliation: Failed to fetch PR details from GitHub: {}. Branch name will be unknown.",
                            e
                        );
                        None
                    }
                };

                let event = Event::ReconciliationFailed {
                    reconciliation_token: token.clone(),
                    error: "No matching batch found at OpenAI".to_string(),
                };

                (event, Some(fallback_pr_url), branch_name)
            }
        };

        // Create an interpreter context for this PR with recovered metadata
        let ctx = InterpreterContext {
            github_client: state.github_client.clone(),
            openai_client: state.openai_client.clone(),
            repo_owner: pr_id.repo_owner.clone(),
            repo_name: pr_id.repo_name.clone(),
            pr_number: pr_id.pr_number,
            pr_url,
            branch_name,
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

    #[test]
    fn test_pr_url_can_be_constructed_from_parts() {
        // Given repo_owner, repo_name, and pr_number, we should always be able
        // to construct a valid PR URL for the InterpreterContext
        let repo_owner = "smaug123";
        let repo_name = "test-repo";
        let pr_number = 42u64;

        let pr_url = format!(
            "https://github.com/{}/{}/pull/{}",
            repo_owner, repo_name, pr_number
        );

        assert_eq!(pr_url, "https://github.com/smaug123/test-repo/pull/42");
    }

    /// Test that branch_name is extracted from batch metadata when available.
    #[test]
    fn test_branch_name_extracted_from_batch_metadata() {
        use std::collections::HashMap;

        // Simulate a batch with branch metadata
        let mut metadata = HashMap::new();
        metadata.insert("branch".to_string(), "feature/my-branch".to_string());
        metadata.insert("reconciliation_token".to_string(), "test-token".to_string());

        // The extract_branch_from_metadata helper should return the branch
        let branch = metadata.get("branch").cloned();
        assert_eq!(branch, Some("feature/my-branch".to_string()));

        // If branch is "<no branch>", treat it as None
        let mut metadata_no_branch = HashMap::new();
        metadata_no_branch.insert("branch".to_string(), "<no branch>".to_string());
        let branch = metadata_no_branch.get("branch").and_then(|b| {
            if b == "<no branch>" {
                None
            } else {
                Some(b.clone())
            }
        });
        assert_eq!(branch, None);
    }

    #[test]
    fn test_is_in_flight_status() {
        // In-flight statuses
        assert!(is_in_flight_status("validating"));
        assert!(is_in_flight_status("in_progress"));
        assert!(is_in_flight_status("finalizing"));

        // Terminal statuses
        assert!(!is_in_flight_status("completed"));
        assert!(!is_in_flight_status("failed"));
        assert!(!is_in_flight_status("cancelled"));
        assert!(!is_in_flight_status("expired"));
        assert!(!is_in_flight_status("cancelling"));
    }

    #[test]
    fn test_should_replace_batch_prefers_in_flight() {
        // In-flight beats terminal, regardless of age
        assert!(should_replace_batch("completed", 200, "in_progress", 100));
        assert!(should_replace_batch("failed", 200, "validating", 100));
        assert!(should_replace_batch("cancelled", 200, "finalizing", 100));

        // Terminal does not beat in-flight, even if newer
        assert!(!should_replace_batch("in_progress", 100, "completed", 200));
        assert!(!should_replace_batch("validating", 100, "failed", 200));
    }

    #[test]
    fn test_should_replace_batch_prefers_newer_within_category() {
        // Among in-flight, prefer newer
        assert!(should_replace_batch("in_progress", 100, "in_progress", 200));
        assert!(!should_replace_batch(
            "in_progress",
            200,
            "in_progress",
            100
        ));

        // Among terminal, prefer newer
        assert!(should_replace_batch("completed", 100, "failed", 200));
        assert!(!should_replace_batch("failed", 200, "completed", 100));
    }

    #[test]
    fn test_should_replace_batch_keeps_in_flight_over_newer_terminal() {
        // Older in-flight should be kept over newer terminal
        assert!(!should_replace_batch("in_progress", 100, "completed", 200));
        assert!(!should_replace_batch("validating", 50, "failed", 300));
    }
}
