use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

use crate::github::{CommitStatusRequest, CommitStatusState, PullRequestInfo};
use crate::openai::BatchResponse;
use crate::{AppState, PendingBatch, COMMIT_STATUS_CONTEXT};

pub async fn batch_polling_loop(state: Arc<AppState>) {
    let mut interval = interval(Duration::from_secs(60)); // Poll every minute

    loop {
        interval.tick().await;

        if let Err(e) = poll_pending_batches(&state).await {
            error!("Error polling batches: {}", e);
        }
    }
}

async fn poll_pending_batches(state: &Arc<AppState>) -> Result<()> {
    let batch_ids: Vec<String> = {
        let pending = state.pending_batches.read().await;
        if pending.is_empty() {
            return Ok(());
        }
        pending.keys().cloned().collect()
    };

    info!("Polling {} pending batches", batch_ids.len());

    for batch_id in batch_ids {
        if let Err(e) = process_single_batch(state, &batch_id).await {
            error!("Error processing batch {}: {}", batch_id, e);
        }
    }

    Ok(())
}

async fn process_single_batch(state: &Arc<AppState>, batch_id: &str) -> Result<()> {
    let batch_response = state.openai_client.get_batch(None, batch_id).await?;

    match batch_response.status.as_str() {
        "completed" => {
            info!("Batch {} completed, processing results", batch_id);

            let pending_batch = {
                let mut pending = state.pending_batches.write().await;
                pending.remove(batch_id)
            };

            if let Some(pending) = pending_batch {
                if let Err(e) = handle_completed_batch(state, batch_response, pending).await {
                    error!("Failed to handle completed batch {}: {}", batch_id, e);
                }
            } else {
                warn!("Completed batch {} not found in pending list", batch_id);
            }
        }
        "failed" | "expired" | "cancelled" => {
            warn!(
                "Batch {} ended with status: {}",
                batch_id, batch_response.status
            );

            let pending_batch = {
                let mut pending = state.pending_batches.write().await;
                pending.remove(batch_id)
            };

            if let Some(pending) = pending_batch {
                let commit_sha = batch_response
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("source_commit"))
                    .map(|s| s.as_str());
                if let Err(e) =
                    handle_failed_batch(state, &batch_response.status, pending, commit_sha).await
                {
                    error!("Failed to handle failed batch {}: {}", batch_id, e);
                }
            }
        }
        _ => {
            // Still in progress (validating, in_progress, finalizing)
            // Keep polling
        }
    }

    Ok(())
}

async fn handle_completed_batch(
    state: &Arc<AppState>,
    batch_response: BatchResponse,
    pending: PendingBatch,
) -> Result<()> {
    info!(
        "Handling completed batch {} for PR #{}",
        batch_response.id, pending.pr_number
    );

    let output_content = if let Some(output_file_id) = &batch_response.output_file_id {
        match state
            .openai_client
            .download_batch_output(None, output_file_id)
            .await
        {
            Ok(content) => content,
            Err(e) => {
                error!("Failed to download batch output: {}", e);
                return handle_failed_batch(state, "download_failed", pending, None).await;
            }
        }
    } else {
        error!("Completed batch {} has no output file", batch_response.id);
        return handle_failed_batch(state, "no_output_file", pending, None).await;
    };

    // Parse the JSONL output
    let review_result = match parse_batch_output(&output_content) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse batch output: {}", e);
            return handle_failed_batch(state, "parse_failed", pending, None).await;
        }
    };

    // Extract commit SHA from batch metadata
    let head_sha = batch_response
        .metadata
        .as_ref()
        .and_then(|m| m.get("source_commit"))
        .map(|s| s.as_str())
        .unwrap_or("unknown");

    // Update the PR comment with the review results
    let final_content = format_review_comment(&review_result, &batch_response.id, head_sha);

    let github_client = &state.github_client;
    let pr_info = PullRequestInfo {
        installation_id: pending.installation_id,
        repo_owner: pending.repo_owner.clone(),
        repo_name: pending.repo_name.clone(),
        pr_number: pending.pr_number,
    };
    match github_client
        .manage_robocop_comment(None, &pr_info, &final_content, &pending.version)
        .await
    {
        Ok(comment_id) => {
            info!(
                "Successfully updated comment {} with review results for batch {}",
                comment_id, batch_response.id
            );

            // Post commit status based on review result
            let status_state = determine_review_status(review_result.substantive_comments);
            let status_description = if review_result.substantive_comments {
                "Code review found issues"
            } else {
                "Code review passed"
            };

            let status_target_url = format!(
                "https://github.com/{}/{}/pull/{}#issuecomment-{}",
                pending.repo_owner, pending.repo_name, pending.pr_number, comment_id
            );

            let status_request = CommitStatusRequest {
                installation_id: pending.installation_id,
                repo_owner: &pending.repo_owner,
                repo_name: &pending.repo_name,
                sha: &pending.head_sha,
                state: status_state,
                target_url: Some(&status_target_url),
                description: Some(status_description),
                context: COMMIT_STATUS_CONTEXT,
            };

            if let Err(e) = github_client
                .post_commit_status(None, &status_request)
                .await
            {
                error!(
                    "Failed to post commit status for completed batch {}: {}",
                    batch_response.id, e
                );
            }
        }
        Err(e) => {
            error!(
                "Failed to update comment for completed batch {}: {}",
                batch_response.id, e
            );
        }
    }

    Ok(())
}

async fn handle_failed_batch(
    state: &Arc<AppState>,
    status: &str,
    pending: PendingBatch,
    commit_sha: Option<&str>,
) -> Result<()> {
    warn!(
        "Handling failed batch {} for PR #{} (status: {})",
        pending.batch_id, pending.pr_number, status
    );

    let commit_info = commit_sha
        .map(|sha| format!("**Commit:** `{}`\n", sha))
        .unwrap_or_default();

    let error_content = format!(
        "❌ **Code review failed**\n\n\
        I encountered an error while analyzing this pull request.\n\n\
        {}**Batch ID:** `{}`\n\
        **Error:** {}\n\n\
        Please try again or contact support if the issue persists.",
        commit_info,
        pending.batch_id,
        match status {
            "failed" => "Analysis failed during processing",
            "expired" => "Analysis timed out (24h limit exceeded)",
            "cancelled" => "Analysis was cancelled",
            "download_failed" => "Failed to retrieve results",
            "no_output_file" => "No output file available",
            "parse_failed" => "Failed to parse results",
            _ => "Unknown error occurred",
        }
    );

    let github_client = &state.github_client;
    let pr_info = PullRequestInfo {
        installation_id: pending.installation_id,
        repo_owner: pending.repo_owner.clone(),
        repo_name: pending.repo_name.clone(),
        pr_number: pending.pr_number,
    };
    if let Err(e) = github_client
        .manage_robocop_comment(None, &pr_info, &error_content, &pending.version)
        .await
    {
        error!(
            "Failed to update comment for failed batch {}: {}",
            pending.batch_id, e
        );
    }

    // Post error commit status
    let status_desc = match status {
        "failed" => "Review failed during processing",
        "expired" => "Review timed out",
        "cancelled" => "Review was cancelled",
        _ => "Review encountered an error",
    };

    let status_request = CommitStatusRequest {
        installation_id: pending.installation_id,
        repo_owner: &pending.repo_owner,
        repo_name: &pending.repo_name,
        sha: &pending.head_sha,
        state: CommitStatusState::Error,
        target_url: None,
        description: Some(status_desc),
        context: COMMIT_STATUS_CONTEXT,
    };

    if let Err(e) = github_client
        .post_commit_status(None, &status_request)
        .await
    {
        error!(
            "Failed to post error commit status for batch {}: {}",
            pending.batch_id, e
        );
    }

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct BatchOutputLine {
    response: BatchOutputResponse,
    error: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchOutputResponse {
    status_code: u16,
    body: BatchOutputBody,
}

#[derive(Debug, serde::Deserialize)]
struct BatchOutputBody {
    choices: Vec<BatchOutputChoice>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchOutputChoice {
    message: BatchOutputMessage,
}

#[derive(Debug, serde::Deserialize)]
struct BatchOutputMessage {
    content: String,
}

#[derive(Debug, serde::Deserialize)]
struct ReviewResult {
    #[serde(rename = "substantiveComments")]
    substantive_comments: bool,
    summary: String,
}

fn parse_batch_output(jsonl_content: &str) -> Result<ReviewResult> {
    // Parse the JSONL - should be exactly one line
    let line = jsonl_content.lines().next().context("Empty batch output")?;

    let batch_output: BatchOutputLine =
        serde_json::from_str(line).context("Failed to parse batch output line")?;

    // Check for errors in the batch response
    if let Some(error) = &batch_output.error {
        return Err(anyhow::anyhow!("Batch request failed: {}", error));
    }

    // Check if response was successful
    if batch_output.response.status_code != 200 {
        return Err(anyhow::anyhow!(
            "Non-200 status code: {}",
            batch_output.response.status_code
        ));
    }

    if batch_output.response.body.choices.is_empty() {
        return Err(anyhow::anyhow!("No choices in batch output"));
    }

    let content = &batch_output.response.body.choices[0].message.content;
    let review_result: ReviewResult =
        serde_json::from_str(content).context("Failed to parse review result JSON")?;

    Ok(review_result)
}

/// Determine the commit status state based on the review result.
///
/// - Returns `Success` if no substantive comments were found
/// - Returns `Failure` if substantive comments were found
fn determine_review_status(substantive_comments: bool) -> CommitStatusState {
    if substantive_comments {
        CommitStatusState::Failure
    } else {
        CommitStatusState::Success
    }
}

fn format_review_comment(review: &ReviewResult, batch_id: &str, commit_sha: &str) -> String {
    if review.substantive_comments {
        format!(
            "🤖 **Code Review Complete**\n\n\
            {}\n\n\
            **Commit:** `{}`\n\
            **Batch ID:** `{}`\n\
            **Status:** Completed",
            review.summary, commit_sha, batch_id
        )
    } else {
        format!(
            "✅ **Code Review Complete**\n\n\
            No issues found in this pull request. The changes look good to merge!\n\n\
            **Commit:** `{}`\n\
            **Batch ID:** `{}`\n\
            **Status:** Completed",
            commit_sha, batch_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_review_status_with_issues() {
        // When there are substantive comments, the status should be Failure
        let status = determine_review_status(true);
        assert_eq!(status, CommitStatusState::Failure);
    }

    #[test]
    fn test_determine_review_status_without_issues() {
        // When there are no substantive comments, the status should be Success
        let status = determine_review_status(false);
        assert_eq!(status, CommitStatusState::Success);
    }

    #[test]
    fn test_format_review_comment_with_issues() {
        let review = ReviewResult {
            substantive_comments: true,
            summary: "Found a potential null pointer issue".to_string(),
        };
        let comment = format_review_comment(&review, "batch_123", "abc1234");

        assert!(comment.contains("🤖 **Code Review Complete**"));
        assert!(comment.contains("Found a potential null pointer issue"));
        assert!(comment.contains("`abc1234`"));
        assert!(comment.contains("`batch_123`"));
    }

    #[test]
    fn test_format_review_comment_without_issues() {
        let review = ReviewResult {
            substantive_comments: false,
            summary: "This shouldn't be shown".to_string(),
        };
        let comment = format_review_comment(&review, "batch_456", "def5678");

        assert!(comment.contains("✅ **Code Review Complete**"));
        assert!(comment.contains("No issues found"));
        assert!(comment.contains("`def5678`"));
        assert!(comment.contains("`batch_456`"));
        // Summary is not shown when no issues
        assert!(!comment.contains("This shouldn't be shown"));
    }

    #[test]
    fn test_parse_batch_output_success() {
        let jsonl = r#"{"response":{"status_code":200,"body":{"choices":[{"message":{"content":"{\"substantiveComments\":false,\"summary\":\"All good\"}"}}]}},"error":null}"#;
        let result = parse_batch_output(jsonl).unwrap();
        assert!(!result.substantive_comments);
        assert_eq!(result.summary, "All good");
    }

    #[test]
    fn test_parse_batch_output_with_issues() {
        let jsonl = r#"{"response":{"status_code":200,"body":{"choices":[{"message":{"content":"{\"substantiveComments\":true,\"summary\":\"Found issues\"}"}}]}},"error":null}"#;
        let result = parse_batch_output(jsonl).unwrap();
        assert!(result.substantive_comments);
        assert_eq!(result.summary, "Found issues");
    }

    #[test]
    fn test_parse_batch_output_empty() {
        let result = parse_batch_output("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_batch_output_non_200() {
        let jsonl = r#"{"response":{"status_code":500,"body":{"choices":[]}},"error":null}"#;
        let result = parse_batch_output(jsonl);
        assert!(result.is_err());
    }
}
