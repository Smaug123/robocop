use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};

use crate::state_machine::{
    batch_completed_event, batch_status_update_event, batch_terminated_event, BatchStatus,
    FailureReason, InterpreterContext, ReviewResult,
};
use crate::AppState;

/// Main batch polling loop that runs in the background.
///
/// This loop polls the state store for pending batches and generates
/// appropriate events when batch status changes.
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
    // Get all pending batches from the state store
    let pending_batches = state.state_store.get_pending_batches().await;

    if pending_batches.is_empty() {
        return Ok(());
    }

    info!("Polling {} pending batches", pending_batches.len());

    for (pr_id, batch_id, installation_id) in pending_batches {
        if let Err(e) = process_single_batch(state, &pr_id, &batch_id, installation_id).await {
            error!(
                "Error processing batch {} for PR #{}: {}",
                &batch_id.0, pr_id.pr_number, e
            );
        }
    }

    Ok(())
}

/// Helper to create interpreter context and process an event.
async fn create_and_process_event(
    state: &Arc<AppState>,
    pr_id: &crate::state_machine::StateMachinePrId,
    event: crate::state_machine::Event,
    installation_id: u64,
) -> Result<()> {
    let pr_url = format!(
        "https://github.com/{}/{}/pull/{}",
        pr_id.repo_owner, pr_id.repo_name, pr_id.pr_number
    );

    // Note: branch_name is only needed during batch submission, not during polling
    let branch_name = None;

    let ctx = InterpreterContext {
        github_client: state.github_client.clone(),
        openai_client: state.openai_client.clone(),
        installation_id,
        repo_owner: pr_id.repo_owner.clone(),
        repo_name: pr_id.repo_name.clone(),
        pr_number: pr_id.pr_number,
        pr_url: Some(pr_url),
        branch_name,
        correlation_id: None,
    };

    let final_state = state
        .state_store
        .process_event(pr_id, event, &ctx)
        .await
        .context("Repository error during batch event processing")?;
    info!(
        "Batch event processed for PR #{}, final state: {:?}",
        pr_id.pr_number, final_state
    );

    Ok(())
}

/// Process a single batch by checking its status and generating the appropriate event.
///
/// This is called by both the polling loop and the OpenAI webhook handler.
pub async fn process_single_batch(
    state: &Arc<AppState>,
    pr_id: &crate::state_machine::StateMachinePrId,
    batch_id: &crate::state_machine::BatchId,
    installation_id: u64,
) -> Result<()> {
    let batch_response = state.openai_client.get_batch(None, &batch_id.0).await?;

    // Parse the status and create appropriate event
    let event = match batch_response.status.as_str() {
        "completed" => {
            info!(
                "Batch {} completed, processing results for PR #{}",
                &batch_id.0, pr_id.pr_number
            );

            // Download the output file
            let output_file_id = match &batch_response.output_file_id {
                Some(id) => id,
                None => {
                    error!("Completed batch {} has no output file", batch_response.id);
                    return create_and_process_event(
                        state,
                        pr_id,
                        batch_terminated_event(&batch_id.0, FailureReason::NoOutputFile),
                        installation_id,
                    )
                    .await;
                }
            };

            let output_content = match state
                .openai_client
                .download_batch_output(None, output_file_id)
                .await
            {
                Ok(content) => content,
                Err(e) => {
                    error!("Failed to download batch output: {}", e);
                    // Emit a failure event so the check run gets updated and a failure
                    // comment is posted. Previously this just returned Ok(()), leaving
                    // the check run stuck in InProgress.
                    return create_and_process_event(
                        state,
                        pr_id,
                        batch_terminated_event(
                            &batch_id.0,
                            FailureReason::DownloadFailed {
                                error: e.to_string(),
                            },
                        ),
                        installation_id,
                    )
                    .await;
                }
            };

            // Parse the output and create appropriate event
            match parse_batch_output(&output_content) {
                Ok(review_result) => batch_completed_event(&batch_id.0, review_result),
                Err(e) => {
                    error!("Failed to parse batch output: {}", e);
                    batch_terminated_event(
                        &batch_id.0,
                        FailureReason::ParseFailed {
                            error: e.to_string(),
                        },
                    )
                }
            }
        }
        "failed" => {
            info!("Batch {} failed for PR #{}", &batch_id.0, pr_id.pr_number);
            batch_terminated_event(&batch_id.0, FailureReason::BatchFailed { error: None })
        }
        "expired" => {
            info!("Batch {} expired for PR #{}", &batch_id.0, pr_id.pr_number);
            batch_terminated_event(&batch_id.0, FailureReason::BatchExpired)
        }
        "cancelled" => {
            // Batch was cancelled - this typically happens when we cancel it ourselves,
            // but handle gracefully in case of external cancellation
            info!(
                "Batch {} was cancelled for PR #{}",
                &batch_id.0, pr_id.pr_number
            );
            batch_terminated_event(&batch_id.0, FailureReason::BatchCancelled)
        }
        status => {
            // Still in progress - send status update
            if let Some(batch_status) = BatchStatus::parse(status) {
                batch_status_update_event(&batch_id.0, batch_status)
            } else {
                // Unknown status - log and skip
                info!(
                    "Unknown batch status '{}' for batch {}, skipping",
                    status, &batch_id.0
                );
                return Ok(());
            }
        }
    };

    // Process the event through the state machine
    create_and_process_event(state, pr_id, event, installation_id).await
}

/// Batch output line from OpenAI responses API
#[derive(Debug, serde::Deserialize)]
struct BatchOutputLine {
    response: BatchOutputResponse,
    error: Option<serde_json::Value>,
}

/// Response wrapper from batch API
#[derive(Debug, serde::Deserialize)]
struct BatchOutputResponse {
    status_code: u16,
    body: ResponsesApiBody,
}

/// Body structure from responses API
#[derive(Debug, serde::Deserialize)]
struct ResponsesApiBody {
    output: Vec<ResponsesApiOutput>,
}

/// Output item from responses API
#[derive(Debug, serde::Deserialize)]
struct ResponsesApiOutput {
    #[serde(rename = "type")]
    output_type: String,
    /// Content is optional because some output types (like reasoning) don't have it
    #[serde(default)]
    content: Vec<ResponsesApiContent>,
}

/// Content item from responses API output
#[derive(Debug, serde::Deserialize)]
struct ResponsesApiContent {
    #[serde(rename = "type")]
    content_type: String,
    /// Text is optional because some content types (e.g., refusal) may not have it
    text: Option<String>,
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

    // Extract content from responses API format
    // Find the message output (there may be other outputs like reasoning)
    let message_output = batch_output
        .response
        .body
        .output
        .iter()
        .find(|o| o.output_type == "message")
        .context("No message output found in response")?;

    // Find the output_text content with text
    let text_content = message_output
        .content
        .iter()
        .find(|c| c.content_type == "output_text" && c.text.is_some())
        .context("No output_text content found in message")?;

    let result: ReviewResult = serde_json::from_str(text_content.text.as_ref().unwrap())
        .context("Failed to parse review result JSON")?;

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_batch_output_success() {
        // Responses API format: output[].content[].text
        // Includes reasoning field as required by the strict schema
        let jsonl = r#"{"response":{"status_code":200,"body":{"output":[{"type":"message","content":[{"type":"output_text","text":"{\"reasoning\":\"No issues found in the code.\",\"substantiveComments\":false,\"summary\":\"All good\"}"}]}]}},"error":null}"#;
        let result = parse_batch_output(jsonl).unwrap();
        assert!(!result.substantive_comments);
        assert_eq!(result.summary, "All good");
        assert_eq!(result.reasoning, "No issues found in the code.");
    }

    #[test]
    fn test_parse_batch_output_with_issues() {
        // Responses API format: output[].content[].text
        // Includes reasoning field as required by the strict schema
        let jsonl = r#"{"response":{"status_code":200,"body":{"output":[{"type":"message","content":[{"type":"output_text","text":"{\"reasoning\":\"Found a potential null pointer dereference.\",\"substantiveComments\":true,\"summary\":\"Found issues\"}"}]}]}},"error":null}"#;
        let result = parse_batch_output(jsonl).unwrap();
        assert!(result.substantive_comments);
        assert_eq!(result.summary, "Found issues");
        assert_eq!(
            result.reasoning,
            "Found a potential null pointer dereference."
        );
    }

    #[test]
    fn test_parse_batch_output_empty() {
        let result = parse_batch_output("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_batch_output_non_200() {
        let jsonl = r#"{"response":{"status_code":500,"body":{"output":[]}},"error":null}"#;
        let result = parse_batch_output(jsonl);
        assert!(result.is_err());
    }
}
