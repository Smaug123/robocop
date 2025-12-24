use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Json, Response},
    routing::post,
    Router,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::command;
use crate::git::GitOps;
use crate::github::{
    CheckRunConclusion, CheckRunOutput, CheckRunStatus, CreateCheckRunRequest, FileContentRequest,
    FileSizeLimits, PullRequestInfo, UpdateCheckRunRequest,
};
use crate::openai::{ReviewMetadata, DEFAULT_MODEL};
use crate::{AppState, CHECK_RUN_NAME};
use crate::{CorrelationId, Direction, EventType, RecordedEvent, Sanitizer};

/// Safely truncate a SHA to at most 7 characters for display.
/// Returns the full string if shorter than 7 characters.
fn truncate_sha(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

/// Build a check run request for when reviews are disabled/suppressed.
/// This creates a completed check with "skipped" conclusion.
fn build_skipped_check_run_request<'a>(
    installation_id: u64,
    repo_owner: &'a str,
    repo_name: &'a str,
    head_sha: &'a str,
    completed_at: &'a str,
) -> CreateCheckRunRequest<'a> {
    CreateCheckRunRequest {
        installation_id,
        repo_owner,
        repo_name,
        name: CHECK_RUN_NAME,
        head_sha,
        details_url: None,
        external_id: None,
        status: Some(CheckRunStatus::Completed),
        started_at: None,
        conclusion: Some(CheckRunConclusion::Skipped),
        completed_at: Some(completed_at),
        output: Some(CheckRunOutput {
            title: "Reviews disabled for this PR".to_string(),
            summary: format!(
                "Code review for commit {} was skipped because reviews are disabled for this PR.",
                truncate_sha(head_sha)
            ),
            text: None,
        }),
    }
}

#[derive(Debug, Deserialize)]
pub struct GitHubWebhookPayload {
    pub action: Option<String>,
    pub pull_request: Option<PullRequest>,
    pub repository: Option<Repository>,
    pub sender: Option<User>,
    pub installation: Option<Installation>,
    pub comment: Option<Comment>,
    pub issue: Option<Issue>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub user: User,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Issue {
    pub number: u64,
    pub pull_request: Option<PullRequestLink>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PullRequestLink {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Installation {
    pub id: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub head: PullRequestRef,
    pub base: PullRequestRef,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PullRequestRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Repository {
    pub name: String,
    pub full_name: String,
    pub owner: User,
}

#[derive(Debug, Deserialize, Clone)]
pub struct User {
    pub id: u64,
    pub login: String,
}

#[derive(Serialize)]
pub struct WebhookResponse {
    pub message: String,
}

type HmacSha256 = Hmac<Sha256>;

fn verify_github_signature(secret: &str, payload: &[u8], signature: &str) -> bool {
    if !signature.starts_with("sha256=") {
        return false;
    }

    let signature_hex = &signature[7..]; // Remove "sha256=" prefix

    // Decode the hex signature to bytes
    let signature_bytes = match hex::decode(signature_hex) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };

    mac.update(payload);

    // Use constant-time verification
    mac.verify_slice(&signature_bytes).is_ok()
}

async fn verify_webhook_signature(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let correlation_id = CorrelationId(Uuid::new_v4().to_string());

    // Extract request parts for recording
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let signature = parts
        .headers
        .get("x-hub-signature-256")
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !verify_github_signature(&state.webhook_secret, &bytes, signature) {
        error!("Invalid webhook signature");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Record the webhook if recording is enabled
    if let Some(ref logger) = state.recording_logger {
        let headers_map = headers_to_hashmap(&parts.headers);
        let webhook_event = RecordedEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            correlation_id: correlation_id.0.clone(),
            event_type: EventType::WebhookReceived,
            direction: Direction::Request,
            operation: "webhook".to_string(),
            data: serde_json::json!({
                "headers": Sanitizer::sanitize_headers(&headers_map),
                "body": serde_json::from_slice::<serde_json::Value>(&bytes)
                    .unwrap_or(serde_json::Value::Null)
            }),
            metadata: HashMap::new(),
        };
        logger.record(webhook_event);
    }

    // Add correlation_id to request extensions for use in handlers and HTTP clients
    let mut new_request = Request::from_parts(parts, axum::body::Body::from(bytes));
    new_request.extensions_mut().insert(correlation_id);

    Ok(next.run(new_request).await)
}

fn headers_to_hashmap(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (name, value) in headers {
        if let Ok(value_str) = value.to_str() {
            map.insert(name.to_string(), value_str.to_string());
        }
    }
    map
}

pub async fn github_webhook_handler(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<WebhookResponse>, StatusCode> {
    info!("Received webhook payload");

    // Extract correlation ID from request extensions for propagation
    let correlation_id = request
        .extensions()
        .get::<CorrelationId>()
        .map(|id| id.0.clone());

    // Extract JSON payload from request body
    let (_parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let payload: GitHubWebhookPayload =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;

    match payload.action.as_deref() {
        Some("opened") | Some("synchronize") | Some("edited") => {
            info!("Processing PR event: {:?}", payload.action);

            if let Some(pr) = &payload.pull_request {
                // For PR events, use sender instead of pusher (pusher is not available in PR events)
                if let Some(sender) = &payload.sender {
                    info!(
                        "PR #{} in {}, action by {}",
                        pr.number,
                        payload
                            .repository
                            .as_ref()
                            .map(|r| &r.full_name)
                            .unwrap_or(&"unknown".to_string()),
                        sender.login
                    );

                    if sender.id == state.target_user_id {
                        info!(
                            "Action from target user detected (ID: {}, username: {}), processing...",
                            sender.id, sender.login
                        );

                        if let Some(repo) = payload.repository.clone() {
                            if let Some(installation) = &payload.installation {
                                // Invalidate cached review state on PR edits to ensure we rehydrate
                                // from the potentially updated PR description
                                if payload.action.as_deref() == Some("edited") {
                                    let pr_id = crate::PullRequestId {
                                        repo_owner: repo.owner.login.clone(),
                                        repo_name: repo.name.clone(),
                                        pr_number: pr.number,
                                    };
                                    state.review_states.write().await.remove(&pr_id);
                                    info!(
                                        "Invalidated cached review state for PR #{} due to edit",
                                        pr.number
                                    );
                                }

                                // Extract review options from PR description if present
                                let review_options = pr
                                    .body
                                    .as_ref()
                                    .and_then(|body| command::extract_review_options(body));

                                if review_options.is_some() {
                                    info!(
                                        "Found review options in PR description: {:?}",
                                        review_options
                                    );
                                }

                                let state_clone = state.clone();
                                let pr_clone = pr.clone();
                                let installation_id = installation.id;
                                let correlation_id_clone = correlation_id.clone();

                                tokio::spawn(async move {
                                    info!("Spawned background task for code review processing");

                                    if let Err(e) = process_code_review(
                                        correlation_id_clone.as_deref(),
                                        state_clone,
                                        installation_id,
                                        repo,
                                        pr_clone,
                                        false, // force_review: false for automatic triggers
                                        review_options,
                                    )
                                    .await
                                    {
                                        error!("Failed to process code review: {}", e);
                                    }
                                });
                            } else {
                                warn!(
                                    "No installation information in payload, skipping code review"
                                );
                            }
                        } else {
                            warn!("No repository information in payload, skipping code review");
                        }
                    } else {
                        info!(
                            "Action from different user (ID: {}, username: {}), ignoring",
                            sender.id, sender.login
                        );
                    }
                } else {
                    warn!("No sender information in payload");
                }
            } else {
                warn!("No pull request information in payload");
            }
        }
        Some("created") => {
            info!("Processing comment event");

            // Check if this is a comment on a pull request
            if let (Some(comment), Some(issue)) = (&payload.comment, &payload.issue) {
                if issue.pull_request.is_some() {
                    info!(
                        "Comment on PR #{} in {}, by {}",
                        issue.number,
                        payload
                            .repository
                            .as_ref()
                            .map(|r| &r.full_name)
                            .unwrap_or(&"unknown".to_string()),
                        comment.user.login
                    );

                    // Parse the comment for robocop commands
                    match command::parse_comment(&comment.body) {
                        command::ParseResult::Command(robocop_command) => {
                            info!("Found robocop command: {}", robocop_command);

                            // SECURITY: Only process commands from the configured target user
                            // to prevent unauthorized users from depleting OpenAI credits
                            if comment.user.id == state.target_user_id {
                                info!(
                                    "Command from target user detected (ID: {}, username: {}), processing...",
                                    comment.user.id, comment.user.login
                                );

                                match robocop_command {
                                    command::RobocopCommand::Review(opts) => {
                                        info!("Processing @smaug123-robocop review command");

                                        if let (Some(repo), Some(installation)) =
                                            (payload.repository.clone(), &payload.installation)
                                        {
                                            let state_clone = state.clone();
                                            let issue_number = issue.number;
                                            let installation_id = installation.id;
                                            let correlation_id_clone = correlation_id.clone();

                                            tokio::spawn(async move {
                                                info!(
                                                    "Spawned background task for manual review of PR #{}",
                                                    issue_number
                                                );

                                                if let Err(e) = process_manual_review(
                                                    correlation_id_clone.as_deref(),
                                                    state_clone,
                                                    installation_id,
                                                    repo,
                                                    issue_number,
                                                    opts,
                                                )
                                                .await
                                                {
                                                    error!(
                                                        "Failed to process manual review: {}",
                                                        e
                                                    );
                                                }
                                            });
                                        } else {
                                            warn!("Missing repository or installation info for review command");
                                        }
                                    }
                                    command::RobocopCommand::Cancel => {
                                        info!("Processing @smaug123-robocop cancel command");

                                        if let (Some(repo), Some(installation)) =
                                            (payload.repository.clone(), &payload.installation)
                                        {
                                            let state_clone = state.clone();
                                            let issue_number = issue.number;
                                            let installation_id = installation.id;
                                            let correlation_id_clone = correlation_id.clone();

                                            tokio::spawn(async move {
                                                info!(
                                                    "Spawned background task to cancel reviews for PR #{}",
                                                    issue_number
                                                );

                                                if let Err(e) = process_cancel_reviews(
                                                    correlation_id_clone.as_deref(),
                                                    state_clone,
                                                    installation_id,
                                                    repo,
                                                    issue_number,
                                                )
                                                .await
                                                {
                                                    error!("Failed to cancel reviews: {}", e);
                                                }
                                            });
                                        } else {
                                            warn!("Missing repository or installation info for cancel command");
                                        }
                                    }
                                    command::RobocopCommand::EnableReviews => {
                                        info!(
                                            "Processing @smaug123-robocop enable-reviews command"
                                        );

                                        if let (Some(repo), Some(installation)) =
                                            (payload.repository.clone(), &payload.installation)
                                        {
                                            let state_clone = state.clone();
                                            let issue_number = issue.number;
                                            let installation_id = installation.id;
                                            let correlation_id_clone = correlation_id.clone();

                                            tokio::spawn(async move {
                                                info!(
                                                    "Spawned background task to enable reviews for PR #{}",
                                                    issue_number
                                                );

                                                if let Err(e) = process_enable_reviews(
                                                    correlation_id_clone.as_deref(),
                                                    state_clone,
                                                    installation_id,
                                                    repo,
                                                    issue_number,
                                                )
                                                .await
                                                {
                                                    error!(
                                                        "Failed to process enable-reviews: {}",
                                                        e
                                                    );
                                                }
                                            });
                                        } else {
                                            warn!("Missing repository or installation info for enable-reviews command");
                                        }
                                    }
                                    command::RobocopCommand::DisableReviews => {
                                        info!(
                                            "Processing @smaug123-robocop disable-reviews command"
                                        );

                                        if let (Some(repo), Some(installation)) =
                                            (payload.repository.clone(), &payload.installation)
                                        {
                                            let state_clone = state.clone();
                                            let issue_number = issue.number;
                                            let installation_id = installation.id;
                                            let correlation_id_clone = correlation_id.clone();

                                            tokio::spawn(async move {
                                                info!(
                                                    "Spawned background task to disable reviews for PR #{}",
                                                    issue_number
                                                );

                                                if let Err(e) = process_disable_reviews(
                                                    correlation_id_clone.as_deref(),
                                                    state_clone,
                                                    installation_id,
                                                    repo,
                                                    issue_number,
                                                )
                                                .await
                                                {
                                                    error!(
                                                        "Failed to process disable-reviews: {}",
                                                        e
                                                    );
                                                }
                                            });
                                        } else {
                                            warn!("Missing repository or installation info for disable-reviews command");
                                        }
                                    }
                                }
                            } else {
                                info!(
                                    "Command from different user (ID: {}, username: {}), ignoring",
                                    comment.user.id, comment.user.login
                                );
                            }
                        }
                        command::ParseResult::UnrecognizedCommand { attempted } => {
                            info!(
                                "Unrecognized command '{}' from user {} (ID: {})",
                                attempted, comment.user.login, comment.user.id
                            );

                            // Only respond to unrecognized commands from the target user
                            if comment.user.id == state.target_user_id {
                                if let (Some(repo), Some(installation)) =
                                    (payload.repository.clone(), &payload.installation)
                                {
                                    let state_clone = state.clone();
                                    let issue_number = issue.number;
                                    let installation_id = installation.id;
                                    let correlation_id_clone = correlation_id.clone();
                                    let attempted_clone = attempted.clone();

                                    tokio::spawn(async move {
                                        info!(
                                            "Spawned background task to respond to unrecognized command on PR #{}",
                                            issue_number
                                        );

                                        if let Err(e) = process_unrecognized_command(
                                            correlation_id_clone.as_deref(),
                                            state_clone,
                                            installation_id,
                                            repo,
                                            issue_number,
                                            &attempted_clone,
                                        )
                                        .await
                                        {
                                            error!(
                                                "Failed to respond to unrecognized command: {}",
                                                e
                                            );
                                        }
                                    });
                                } else {
                                    warn!("Missing repository or installation info for unrecognized command response");
                                }
                            }
                        }
                        command::ParseResult::NoMention => {
                            // Comment doesn't mention the bot, ignore it
                        }
                    }
                } else {
                    info!("Comment is on an issue, not a PR, ignoring");
                }
            } else {
                warn!("Comment event missing comment or issue data");
            }
        }
        _ => {
            info!("Ignoring webhook event: {:?}", payload.action);
        }
    }

    Ok(Json(WebhookResponse {
        message: "Webhook received".to_string(),
    }))
}

async fn process_cancel_reviews(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Cancelling all pending reviews for PR #{} in {}",
        pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;

    // Get all pending batches for this PR
    let batches_to_cancel: Vec<(String, crate::PendingBatch)> = {
        let pending = state.pending_batches.read().await;
        pending
            .iter()
            .filter(|(_, batch)| {
                batch.pr_number == pr_number
                    && batch.repo_owner == *repo_owner
                    && batch.repo_name == *repo_name
            })
            .map(|(id, batch)| (id.clone(), batch.clone()))
            .collect()
    };

    if batches_to_cancel.is_empty() {
        info!("No pending batches found for PR #{}", pr_number);

        // Post a comment indicating there are no reviews to cancel
        let version = crate::get_bot_version();
        let no_reviews_content = "ℹ️ **No reviews to cancel**\n\n\
            There are no pending reviews for this pull request."
            .to_string();

        let pr_info = PullRequestInfo {
            installation_id,
            repo_owner: repo_owner.to_string(),
            repo_name: repo_name.to_string(),
            pr_number,
        };

        state
            .github_client
            .manage_robocop_comment(correlation_id, &pr_info, &no_reviews_content, &version)
            .await?;

        return Ok(());
    }

    info!(
        "Found {} pending batches to cancel for PR #{}",
        batches_to_cancel.len(),
        pr_number
    );

    let mut cancelled_count = 0;
    let mut failed_cancellations = Vec::new();
    let mut batches_to_remove = Vec::new();

    for (batch_id, batch) in &batches_to_cancel {
        info!("Attempting to cancel batch {}", batch_id);

        match state
            .openai_client
            .cancel_batch(correlation_id, batch_id)
            .await
        {
            Ok(cancel_response) => {
                info!(
                    "Successfully cancelled batch {} (status: {})",
                    batch_id, cancel_response.status
                );
                cancelled_count += 1;
                batches_to_remove.push(batch_id.clone());

                // Update check run with skipped status for user-cancelled batch
                if batch.check_run_id != 0 {
                    let completed_at = chrono::Utc::now().to_rfc3339();
                    let update_request = UpdateCheckRunRequest {
                        installation_id,
                        repo_owner,
                        repo_name,
                        check_run_id: batch.check_run_id,
                        name: Some(CHECK_RUN_NAME),
                        details_url: None,
                        external_id: None,
                        status: Some(CheckRunStatus::Completed),
                        started_at: None,
                        conclusion: Some(CheckRunConclusion::Skipped),
                        completed_at: Some(&completed_at),
                        output: Some(CheckRunOutput {
                            title: "Review cancelled by user".to_string(),
                            summary: format!(
                                "Code review for commit {} was cancelled by user request.",
                                truncate_sha(&batch.head_sha)
                            ),
                            text: None,
                        }),
                    };
                    if let Err(e) = state
                        .github_client
                        .update_check_run(correlation_id, &update_request)
                        .await
                    {
                        error!(
                            "Failed to update check run for cancelled batch {}: {}",
                            batch_id, e
                        );
                    }
                }
            }
            Err(e) => {
                warn!("Failed to cancel batch {}: {}", batch_id, e);
                failed_cancellations.push((batch_id.clone(), e.to_string()));

                // Check if batch is already completed/failed/cancelled
                match state
                    .openai_client
                    .get_batch(correlation_id, batch_id)
                    .await
                {
                    Ok(status_response) => {
                        if matches!(
                            status_response.status.as_str(),
                            "completed" | "failed" | "cancelled" | "expired"
                        ) {
                            info!(
                                "Batch {} is already in terminal state: {}",
                                batch_id, status_response.status
                            );
                            cancelled_count += 1; // Count as cancelled since it won't be processed
                            batches_to_remove.push(batch_id.clone());

                            // Update check run even though cancel failed - batch is terminal
                            if batch.check_run_id != 0 {
                                let completed_at = chrono::Utc::now().to_rfc3339();
                                let update_request = UpdateCheckRunRequest {
                                    installation_id,
                                    repo_owner,
                                    repo_name,
                                    check_run_id: batch.check_run_id,
                                    name: Some(CHECK_RUN_NAME),
                                    details_url: None,
                                    external_id: None,
                                    status: Some(CheckRunStatus::Completed),
                                    started_at: None,
                                    conclusion: Some(CheckRunConclusion::Skipped),
                                    completed_at: Some(&completed_at),
                                    output: Some(CheckRunOutput {
                                        title: "Review cancelled by user".to_string(),
                                        summary: format!(
                                            "Code review for commit {} was cancelled by user request (batch was already {}).",
                                            truncate_sha(&batch.head_sha),
                                            status_response.status
                                        ),
                                        text: None,
                                    }),
                                };
                                if let Err(e) = state
                                    .github_client
                                    .update_check_run(correlation_id, &update_request)
                                    .await
                                {
                                    error!(
                                        "Failed to update check run for terminal batch {}: {}",
                                        batch_id, e
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to check status of batch {}: {}", batch_id, e);
                    }
                }
            }
        }
    }

    // Remove only successfully cancelled or terminal batches from tracking
    {
        let mut pending = state.pending_batches.write().await;
        for batch_id in &batches_to_remove {
            pending.remove(batch_id);
        }
    }

    info!(
        "Cancelled {}/{} batches for PR #{}",
        cancelled_count,
        batches_to_cancel.len(),
        pr_number
    );

    // Post a comment with cancellation results
    let version = crate::get_bot_version();
    let cancellation_content = if failed_cancellations.is_empty() {
        format!(
            "✅ **Reviews cancelled**\n\n\
            Successfully cancelled {} pending review{}.",
            cancelled_count,
            if cancelled_count == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "⚠️ **Reviews partially cancelled**\n\n\
            Successfully cancelled {}/{} pending reviews.\n\n\
            **Failed cancellations:**\n{}",
            cancelled_count,
            batches_to_cancel.len(),
            failed_cancellations
                .iter()
                .map(|(id, err)| format!("- Batch `{}`: {}", id, err))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &cancellation_content, &version)
        .await?;

    Ok(())
}

async fn process_enable_reviews(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Enabling reviews for PR #{} in {}",
        pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };

    // Update state
    {
        let mut states = state.review_states.write().await;
        states.insert(pr_id, crate::ReviewState::Enabled);
    }

    // Fetch PR details to get current commit
    let pr_details = state
        .github_client
        .get_pull_request(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            pr_number,
        )
        .await?;

    // Post acknowledgment comment with current commit info
    let version = crate::get_bot_version();
    let content = format!(
        "✅ **Reviews enabled**\n\n\
        Automatic reviews have been enabled for this PR.\n\n\
        Submitting review for current commit `{}`...",
        pr_details.head.sha
    );

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &content, &version)
        .await?;

    // Trigger a review for the current commit
    // Extract review options from PR body if present
    let review_options = pr_details
        .body
        .as_ref()
        .and_then(|body| command::extract_review_options(body));

    let pr = PullRequest {
        number: pr_details.number,
        head: PullRequestRef {
            sha: pr_details.head.sha,
            ref_name: pr_details.head.ref_name,
        },
        base: PullRequestRef {
            sha: pr_details.base.sha,
            ref_name: pr_details.base.ref_name,
        },
        body: pr_details.body,
    };

    // Use force_review=false since we just enabled reviews
    process_code_review(
        correlation_id,
        state,
        installation_id,
        repo,
        pr,
        false,
        review_options,
    )
    .await
}

async fn process_disable_reviews(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Disabling reviews for PR #{} in {}",
        pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };

    // Update state
    {
        let mut states = state.review_states.write().await;
        states.insert(pr_id, crate::ReviewState::Disabled);
    }

    // Cancel any pending reviews for this PR
    let batches_to_cancel: Vec<(String, crate::PendingBatch)> = {
        let pending = state.pending_batches.read().await;
        pending
            .iter()
            .filter(|(_, batch)| {
                batch.pr_number == pr_number
                    && batch.repo_owner == *repo_owner
                    && batch.repo_name == *repo_name
            })
            .map(|(id, batch)| (id.clone(), batch.clone()))
            .collect()
    };

    let mut batches_to_remove = Vec::new();

    for (batch_id, batch) in &batches_to_cancel {
        info!("Cancelling batch {} due to disable-reviews", batch_id);

        let should_update_check_run;
        let should_remove_from_tracking;

        match state
            .openai_client
            .cancel_batch(correlation_id, batch_id)
            .await
        {
            Ok(_) => {
                should_update_check_run = true;
                should_remove_from_tracking = true;
            }
            Err(e) => {
                warn!("Failed to cancel batch {}: {}", batch_id, e);

                // Check if batch is already in a terminal state
                match state
                    .openai_client
                    .get_batch(correlation_id, batch_id)
                    .await
                {
                    Ok(status_response) => {
                        if matches!(
                            status_response.status.as_str(),
                            "completed" | "failed" | "cancelled" | "expired"
                        ) {
                            info!(
                                "Batch {} is already in terminal state: {}",
                                batch_id, status_response.status
                            );
                            // Terminal state: update check run and remove from tracking
                            should_update_check_run = true;
                            should_remove_from_tracking = true;
                        } else {
                            // Not terminal: don't remove, let polling handle it
                            info!(
                                "Batch {} is still in state {}, leaving in tracking for polling",
                                batch_id, status_response.status
                            );
                            should_update_check_run = false;
                            should_remove_from_tracking = false;
                        }
                    }
                    Err(status_e) => {
                        error!("Failed to check status of batch {}: {}", batch_id, status_e);
                        // Can't determine state: don't remove, let polling handle it
                        should_update_check_run = false;
                        should_remove_from_tracking = false;
                    }
                }
            }
        }

        // Update check run with skipped status for disabled review
        if should_update_check_run && batch.check_run_id != 0 {
            let completed_at = chrono::Utc::now().to_rfc3339();
            let update_request = UpdateCheckRunRequest {
                installation_id,
                repo_owner,
                repo_name,
                check_run_id: batch.check_run_id,
                name: Some(CHECK_RUN_NAME),
                details_url: None,
                external_id: None,
                status: Some(CheckRunStatus::Completed),
                started_at: None,
                conclusion: Some(CheckRunConclusion::Skipped),
                completed_at: Some(&completed_at),
                output: Some(CheckRunOutput {
                    title: "Reviews disabled for this PR".to_string(),
                    summary: format!(
                        "Code review for commit {} was skipped because reviews are disabled for this PR.",
                        truncate_sha(&batch.head_sha)
                    ),
                    text: None,
                }),
            };
            if let Err(e) = state
                .github_client
                .update_check_run(correlation_id, &update_request)
                .await
            {
                error!(
                    "Failed to update check run for disabled batch {}: {}",
                    batch_id, e
                );
            }
        }

        if should_remove_from_tracking {
            batches_to_remove.push(batch_id.clone());
        }
    }

    let cancelled_count = batches_to_remove.len();

    // Remove only successfully cancelled or terminal batches from tracking
    {
        let mut pending = state.pending_batches.write().await;
        for batch_id in &batches_to_remove {
            pending.remove(batch_id);
        }
    }

    // Post acknowledgment
    let version = crate::get_bot_version();
    let content = if cancelled_count > 0 {
        format!(
            "🔕 **Reviews disabled**\n\n\
            Automatic reviews have been disabled for this PR.\n\n\
            Cancelled {} pending review{}.",
            cancelled_count,
            if cancelled_count == 1 { "" } else { "s" }
        )
    } else {
        "🔕 **Reviews disabled**\n\n\
        Automatic reviews have been disabled for this PR."
            .to_string()
    };

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &content, &version)
        .await?;

    Ok(())
}

async fn process_unrecognized_command(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
    attempted_command: &str,
) -> anyhow::Result<()> {
    info!(
        "Responding to unrecognized command '{}' on PR #{} in {}",
        attempted_command, pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;

    let version = crate::get_bot_version();
    let attempted_display = if attempted_command.is_empty() {
        "(no command provided)".to_string()
    } else {
        format!("`{}`", attempted_command)
    };

    let content = format!(
        "❓ **Unrecognized command**\n\n\
        I didn't recognize the command {}.\n\n\
        **Available commands:**\n\
        - `@smaug123-robocop review` — Request a code review\n\
        - `@smaug123-robocop review model:<model> reasoning:<level>` — Review with custom options\n\
        - `@smaug123-robocop cancel` — Cancel all pending reviews for this PR\n\
        - `@smaug123-robocop enable-reviews` — Enable automatic reviews\n\
        - `@smaug123-robocop disable-reviews` — Disable automatic reviews",
        attempted_display
    );

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &content, &version)
        .await?;

    Ok(())
}

async fn process_manual_review(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
    review_options: command::ReviewOptions,
) -> anyhow::Result<()> {
    info!(
        "Processing manual review request for PR #{} in {} (model: {:?}, reasoning: {:?})",
        pr_number, repo.full_name, review_options.model, review_options.reasoning_effort
    );

    let github_client = &state.github_client;

    // Fetch PR details to get head and base SHAs
    let pr_details = github_client
        .get_pull_request(
            correlation_id,
            installation_id,
            &repo.owner.login,
            &repo.name,
            pr_number,
        )
        .await?;

    // Convert to PullRequest type for process_code_review
    let pr = PullRequest {
        number: pr_details.number,
        head: PullRequestRef {
            sha: pr_details.head.sha,
            ref_name: pr_details.head.ref_name,
        },
        base: PullRequestRef {
            sha: pr_details.base.sha,
            ref_name: pr_details.base.ref_name,
        },
        body: pr_details.body,
    };

    // Use the existing code review logic with force flag and review options
    process_code_review(
        correlation_id,
        state,
        installation_id,
        repo,
        pr,
        true,
        Some(review_options),
    )
    .await
}

async fn process_code_review(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr: PullRequest,
    force_review: bool,
    review_options: Option<command::ReviewOptions>,
) -> anyhow::Result<()> {
    info!(
        "Processing code review for PR #{} in {} (force: {}, options: {:?})",
        pr.number, repo.full_name, force_review, review_options
    );

    let github_client = &state.github_client;
    let openai_client = &state.openai_client;

    // Extract repository info
    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let base_sha = &pr.base.sha;
    let head_sha = &pr.head.sha;
    let branch_name = Some(pr.head.ref_name.as_str());

    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number: pr.number,
    };

    // Get or rehydrate review state
    let review_state = {
        let states = state.review_states.read().await;
        if let Some(&cached_state) = states.get(&pr_id) {
            cached_state
        } else {
            drop(states); // Release read lock before expensive operation

            // Rehydrate state from PR history
            let rehydrated_state = crate::review_state::rehydrate_review_state(
                github_client,
                correlation_id,
                installation_id,
                repo_owner,
                repo_name,
                pr.number,
                state.target_user_id,
            )
            .await?;

            // Store it for future use
            let mut states = state.review_states.write().await;
            states.insert(pr_id.clone(), rehydrated_state);
            rehydrated_state
        }
    };

    // Check if reviews are suppressed (unless forced by explicit command)
    if review_state == crate::ReviewState::Disabled && !force_review {
        info!(
            "Reviews are disabled for PR #{}, posting suppression notice",
            pr.number
        );

        let version = crate::get_bot_version();
        let suppression_content = format!(
            "ℹ️ **Review suppressed**\n\n\
            Not reviewing commit `{}` due to explicit suppression.\n\n\
            To enable reviews, comment `@smaug123-robocop enable-reviews` or \
            request a one-time review with `@smaug123-robocop review`.",
            pr.head.sha
        );

        let pr_info = PullRequestInfo {
            installation_id,
            repo_owner: repo_owner.to_string(),
            repo_name: repo_name.to_string(),
            pr_number: pr.number,
        };

        github_client
            .manage_robocop_comment(correlation_id, &pr_info, &suppression_content, &version)
            .await?;

        // Create a skipped check run so the PR shows the review was intentionally skipped
        let completed_at = chrono::Utc::now().to_rfc3339();
        let check_run_request = build_skipped_check_run_request(
            installation_id,
            repo_owner,
            repo_name,
            head_sha,
            &completed_at,
        );

        if let Err(e) = github_client
            .create_check_run(correlation_id, &check_run_request)
            .await
        {
            // Log but don't fail - check run creation is best-effort
            error!(
                "Failed to create skipped check run for PR #{}: {}",
                pr.number, e
            );
        }

        return Ok(());
    }

    // Get the diff
    let diff = github_client
        .get_diff(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            base_sha,
            head_sha,
        )
        .await?;

    if diff.trim().is_empty() {
        info!("No diff found, skipping code review");
        return Ok(());
    }

    // Get list of changed files
    let changed_files = github_client
        .get_changed_files_from_diff(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            base_sha,
            head_sha,
        )
        .await?;

    if changed_files.is_empty() {
        info!("No changed files found, skipping code review");
        return Ok(());
    }

    // Get file contents with size limits (100KB per file, 1MB total)
    let limits = FileSizeLimits {
        max_file_size: 100_000,    // 100KB
        max_total_size: 1_000_000, // 1MB
    };

    let request = FileContentRequest {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        file_paths: changed_files,
        sha: head_sha.to_string(),
    };

    let (file_contents, skipped_files) = github_client
        .get_multiple_file_contents_with_limits(correlation_id, &request, &limits)
        .await?;

    // Check if diff is too big for review
    if file_contents.is_empty() && !skipped_files.is_empty() {
        info!("All files were skipped due to size limits, posting skipped comment");

        let version = crate::get_bot_version();
        let total_files = request.file_paths.len();
        let skipped_content = format!(
            "🤖 **Review skipped: Diff too large**\n\n\
            This pull request contains changes that are too large for automated review:\n\n\
            **Files changed:** {}\n\
            **Files skipped:** {}\n\n\
            **Skipped files:**\n{}\n\n\
            **Limits:** Max 100KB per file, 1MB total\n\n\
            Please consider splitting this into smaller, more focused changes for automated review.",
            total_files,
            skipped_files.len(),
            {
                let mut file_list = skipped_files.iter()
                    .take(10)
                    .map(|f| format!("- {}", f))
                    .collect::<Vec<_>>()
                    .join("\n");
                if skipped_files.len() > 10 {
                    file_list.push_str(&format!("\n- ... and {} more files", skipped_files.len() - 10));
                }
                file_list
            }
        );

        let pr_info = PullRequestInfo {
            installation_id,
            repo_owner: repo_owner.to_string(),
            repo_name: repo_name.to_string(),
            pr_number: pr.number,
        };

        github_client
            .manage_robocop_comment(correlation_id, &pr_info, &skipped_content, &version)
            .await?;

        info!(
            "Posted skip comment due to size limits for PR #{} in {}",
            pr.number, repo.full_name
        );
        return Ok(());
    }

    if file_contents.is_empty() {
        info!("No file contents could be retrieved, skipping code review");
        return Ok(());
    }

    // Log if some files were skipped but we can still proceed
    if !skipped_files.is_empty() {
        info!(
            "Proceeding with partial review: {} files downloaded, {} skipped",
            file_contents.len(),
            skipped_files.len()
        );
    }

    // Check for existing batches that should be cancelled due to commit ancestry
    cancel_superseded_batches(
        correlation_id,
        &state,
        installation_id,
        repo_owner,
        repo_name,
        pr.number,
        head_sha,
    )
    .await?;

    // Process with OpenAI batch API
    let pull_request_url = format!("https://github.com/{}/pull/{}", repo.full_name, pr.number);
    let metadata = ReviewMetadata {
        head_hash: head_sha.to_string(),
        merge_base: base_sha.to_string(),
        branch_name: branch_name.map(|s| s.to_string()),
        repo_name: repo.name.clone(),
        remote_url: None, // Could extract this from repo data if needed
        pull_request_url: Some(pull_request_url),
    };

    let version = crate::get_bot_version();

    // Extract options or use defaults
    let reasoning_effort = review_options
        .as_ref()
        .and_then(|opts| opts.reasoning_effort.as_deref())
        .unwrap_or("xhigh");
    let model = review_options
        .as_ref()
        .and_then(|opts| opts.model.as_deref());

    let batch_id = openai_client
        .process_code_review_batch(
            correlation_id,
            &diff,
            &file_contents,
            &metadata,
            reasoning_effort,
            Some(&version),
            None, // additional_prompt
            model,
        )
        .await?;

    // Determine the actual model used (specified or default)
    let actual_model = model.unwrap_or(DEFAULT_MODEL);

    info!(
        "Successfully submitted batch request {} for PR #{} in {}",
        batch_id, pr.number, repo.full_name
    );
    let in_progress_content = format!(
        "🤖 **Code review in progress...**\n\n\
        I'm analyzing the changes in this pull request. This may take a long time depending on current OpenAI load.\n\n\
        **Commit:** `{}`\n\
        **Batch ID:** `{}`\n\
        **Model:** `{}`\n\
        **Reasoning effort:** `{}`\n\
        **Status:** Processing",
        head_sha, batch_id, actual_model, reasoning_effort
    );

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number: pr.number,
    };
    let comment_id = github_client
        .manage_robocop_comment(correlation_id, &pr_info, &in_progress_content, &version)
        .await?;

    info!(
        "Posted in-progress comment {} for batch {} on PR #{} in {}",
        comment_id, batch_id, pr.number, repo.full_name
    );

    // Create check run with in_progress status
    let check_run_url = format!(
        "https://github.com/{}/pull/{}#issuecomment-{}",
        repo.full_name, pr.number, comment_id
    );
    let started_at = chrono::Utc::now().to_rfc3339();
    let check_run_request = CreateCheckRunRequest {
        installation_id,
        repo_owner,
        repo_name,
        name: CHECK_RUN_NAME,
        head_sha,
        details_url: Some(&check_run_url),
        external_id: Some(&batch_id),
        status: Some(CheckRunStatus::InProgress),
        started_at: Some(&started_at),
        conclusion: None,
        completed_at: None,
        output: Some(CheckRunOutput {
            title: "Code review in progress".to_string(),
            summary: format!("Reviewing commit {}", truncate_sha(head_sha)),
            text: None,
        }),
    };
    let check_run_id = match github_client
        .create_check_run(correlation_id, &check_run_request)
        .await
    {
        Ok(response) => response.id,
        Err(e) => {
            // Log but don't fail the review - check run creation is best-effort
            error!("Failed to create check run for PR #{}: {}", pr.number, e);
            // Use 0 as a sentinel value indicating no check run was created
            0
        }
    };

    // Store batch for polling
    let pending_batch = crate::PendingBatch {
        batch_id: batch_id.clone(),
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number: pr.number,
        comment_id,
        check_run_id,
        version: version.clone(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
    };

    {
        let mut pending = state.pending_batches.write().await;
        pending.insert(batch_id.clone(), pending_batch);
    }

    info!(
        "Added batch {} to polling queue for PR #{} in {}",
        batch_id, pr.number, repo.full_name
    );

    Ok(())
}

async fn cancel_superseded_batches(
    correlation_id: Option<&str>,
    state: &Arc<AppState>,
    installation_id: u64,
    repo_owner: &str,
    repo_name: &str,
    pr_number: u64,
    new_head_sha: &str,
) -> anyhow::Result<()> {
    info!(
        "Checking for batches to cancel due to new commit {} on PR #{} in {}/{}",
        new_head_sha, pr_number, repo_owner, repo_name
    );

    // Get all pending batches for this PR
    let batches_to_check: Vec<(String, crate::PendingBatch)> = {
        let pending = state.pending_batches.read().await;
        pending
            .iter()
            .filter(|(_, batch)| {
                batch.pr_number == pr_number
                    && batch.repo_owner == repo_owner
                    && batch.repo_name == repo_name
            })
            .map(|(id, batch)| (id.clone(), batch.clone()))
            .collect()
    };

    if batches_to_check.is_empty() {
        info!("No existing batches found for PR #{}", pr_number);
        return Ok(());
    }

    info!(
        "Found {} existing batches for PR #{}, checking for superseded commits",
        batches_to_check.len(),
        pr_number
    );

    let mut cancelled_batches = Vec::new();

    for (batch_id, batch) in batches_to_check {
        // Skip if it's the same commit (shouldn't happen in practice)
        if batch.head_sha == new_head_sha {
            continue;
        }

        // Check if the existing commit is an ancestor of the new commit
        // is_ancestor(new_sha, existing_sha) returns true when existing_sha is an ancestor of new_sha
        // In this case: is batch.head_sha (old commit) an ancestor of new_head_sha (new commit)?
        // If true, the old batch is superseded by the new commit and should be cancelled.
        let is_ancestor_result = {
            let github_client = &state.github_client;
            GitOps::is_ancestor(
                github_client,
                installation_id,
                repo_owner,
                repo_name,
                new_head_sha,    // The new commit (descendant)
                &batch.head_sha, // The old commit (potential ancestor)
            )
            .await
        };

        match is_ancestor_result {
            Ok(true) => {
                info!(
                    "Commit {} is superseded by {}, cancelling batch {}",
                    batch.head_sha, new_head_sha, batch_id
                );

                // Attempt to cancel the batch
                match state
                    .openai_client
                    .cancel_batch(correlation_id, &batch_id)
                    .await
                {
                    Ok(cancel_response) => {
                        info!(
                            "Successfully cancelled batch {} (status: {})",
                            batch_id, cancel_response.status
                        );

                        // Update the PR comment to show cancellation
                        let cancellation_content = format!(
                            "❌ **Code review cancelled**\n\n\
                            This review was cancelled because a newer commit superseded it.\n\n\
                            **Cancelled Commit:** `{}`\n\
                            **Superseded by:** `{}`\n\
                            **Batch ID:** `{}`\n\
                            **Status:** Cancelled",
                            batch.head_sha, new_head_sha, batch_id
                        );

                        let pr_info = PullRequestInfo {
                            installation_id: batch.installation_id,
                            repo_owner: batch.repo_owner.clone(),
                            repo_name: batch.repo_name.clone(),
                            pr_number: batch.pr_number,
                        };
                        let github_client = &state.github_client;
                        if let Err(e) = github_client
                            .manage_robocop_comment(
                                correlation_id,
                                &pr_info,
                                &cancellation_content,
                                &batch.version,
                            )
                            .await
                        {
                            error!(
                                "Failed to update comment for cancelled batch {}: {}",
                                batch_id, e
                            );
                        }

                        // Update check run with stale status for superseded commit
                        if batch.check_run_id != 0 {
                            let completed_at = chrono::Utc::now().to_rfc3339();
                            let update_request = UpdateCheckRunRequest {
                                installation_id: batch.installation_id,
                                repo_owner: &batch.repo_owner,
                                repo_name: &batch.repo_name,
                                check_run_id: batch.check_run_id,
                                name: Some(CHECK_RUN_NAME),
                                details_url: None,
                                external_id: None,
                                status: Some(CheckRunStatus::Completed),
                                started_at: None,
                                conclusion: Some(CheckRunConclusion::Stale),
                                completed_at: Some(&completed_at),
                                output: Some(CheckRunOutput {
                                    title: format!(
                                        "Review superseded by commit {}",
                                        truncate_sha(new_head_sha)
                                    ),
                                    summary: format!(
                                        "This review was cancelled because commit {} superseded commit {}.",
                                        truncate_sha(new_head_sha),
                                        truncate_sha(&batch.head_sha)
                                    ),
                                    text: None,
                                }),
                            };
                            if let Err(e) = github_client
                                .update_check_run(correlation_id, &update_request)
                                .await
                            {
                                error!(
                                    "Failed to update check run for superseded batch {}: {}",
                                    batch_id, e
                                );
                            }
                        }

                        cancelled_batches.push(batch_id.clone());
                    }
                    Err(e) => {
                        warn!(
                            "Failed to cancel batch {} (it may already be completed): {}",
                            batch_id, e
                        );

                        // Check the current status of the batch
                        match state
                            .openai_client
                            .get_batch(correlation_id, &batch_id)
                            .await
                        {
                            Ok(status_response) => {
                                info!(
                                    "Batch {} current status: {}",
                                    batch_id, status_response.status
                                );

                                // If it's already completed/failed/cancelled, remove it from tracking
                                if matches!(
                                    status_response.status.as_str(),
                                    "completed" | "failed" | "cancelled" | "expired"
                                ) {
                                    // Update check run even though cancel failed - batch is terminal
                                    if batch.check_run_id != 0 {
                                        let github_client = &state.github_client;
                                        let completed_at = chrono::Utc::now().to_rfc3339();
                                        let update_request = UpdateCheckRunRequest {
                                            installation_id: batch.installation_id,
                                            repo_owner: &batch.repo_owner,
                                            repo_name: &batch.repo_name,
                                            check_run_id: batch.check_run_id,
                                            name: Some(CHECK_RUN_NAME),
                                            details_url: None,
                                            external_id: None,
                                            status: Some(CheckRunStatus::Completed),
                                            started_at: None,
                                            conclusion: Some(CheckRunConclusion::Stale),
                                            completed_at: Some(&completed_at),
                                            output: Some(CheckRunOutput {
                                                title: format!(
                                                    "Review superseded by commit {}",
                                                    truncate_sha(new_head_sha)
                                                ),
                                                summary: format!(
                                                    "This review was cancelled because commit {} superseded commit {} (batch was already {}).",
                                                    truncate_sha(new_head_sha),
                                                    truncate_sha(&batch.head_sha),
                                                    status_response.status
                                                ),
                                                text: None,
                                            }),
                                        };
                                        if let Err(e) = github_client
                                            .update_check_run(correlation_id, &update_request)
                                            .await
                                        {
                                            error!(
                                                "Failed to update check run for terminal superseded batch {}: {}",
                                                batch_id, e
                                            );
                                        }
                                    }

                                    cancelled_batches.push(batch_id.clone());
                                }
                            }
                            Err(status_e) => {
                                error!(
                                    "Failed to check status of batch {}: {}",
                                    batch_id, status_e
                                );
                            }
                        }
                    }
                }
            }
            Ok(false) => {
                info!(
                    "Commit {} is not superseded by {} (parallel development), keeping batch {}",
                    batch.head_sha, new_head_sha, batch_id
                );
            }
            Err(e) => {
                warn!(
                    "Failed to check ancestry between {} and {}: {}",
                    batch.head_sha, new_head_sha, e
                );
            }
        }
    }

    // Remove cancelled batches from tracking
    if !cancelled_batches.is_empty() {
        let mut pending = state.pending_batches.write().await;
        for batch_id in &cancelled_batches {
            pending.remove(batch_id);
        }
        info!(
            "Removed {} cancelled batches from tracking",
            cancelled_batches.len()
        );
    }

    Ok(())
}

pub fn webhook_router(middleware_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/webhook", post(github_webhook_handler))
        .route_layer(middleware::from_fn_with_state(
            middleware_state,
            verify_webhook_signature,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create_comment_webhook_payload(
        action: &str,
        comment_body: &str,
        comment_user_id: u64,
        comment_user_login: &str,
        pr_number: u64,
    ) -> GitHubWebhookPayload {
        GitHubWebhookPayload {
            action: Some(action.to_string()),
            pull_request: None,
            repository: Some(Repository {
                name: "test-repo".to_string(),
                full_name: "test-owner/test-repo".to_string(),
                owner: User {
                    id: 12345,
                    login: "test-owner".to_string(),
                },
            }),
            sender: Some(User {
                id: comment_user_id,
                login: comment_user_login.to_string(),
            }),
            installation: Some(Installation { id: 67890 }),
            comment: Some(Comment {
                id: 999,
                body: comment_body.to_string(),
                user: User {
                    id: comment_user_id,
                    login: comment_user_login.to_string(),
                },
            }),
            issue: Some(Issue {
                number: pr_number,
                pull_request: Some(PullRequestLink {
                    url: format!(
                        "https://api.github.com/repos/test-owner/test-repo/pulls/{}",
                        pr_number
                    ),
                }),
            }),
        }
    }

    #[test]
    fn test_comment_command_parsing_with_authorization() {
        // This test verifies that comment payloads with robocop commands are correctly parsed
        // and that we have the necessary fields to perform authorization checks.
        //
        // SECURITY REQUIREMENT: Only the configured target_user_id should be able to
        // trigger commands via PR comments. This prevents unauthorized users from
        // depleting OpenAI credits.
        //
        // This test verifies we can:
        // 1. Parse a comment payload with a robocop command
        // 2. Extract the comment user ID for authorization
        // 3. Detect valid robocop commands

        // Create a payload from the target user
        let target_user_id = 12345u64;
        let payload_from_target = create_comment_webhook_payload(
            "created",
            "@smaug123-robocop review",
            target_user_id,
            "authorized-user",
            789,
        );

        // Create a payload from a different user
        let unauthorized_user_id = 99999u64;
        let payload_from_unauthorized = create_comment_webhook_payload(
            "created",
            "@smaug123-robocop review",
            unauthorized_user_id,
            "unauthorized-user",
            789,
        );

        // Verify we can parse commands and extract user IDs
        assert_eq!(payload_from_target.action, Some("created".to_string()));
        let comment_target = payload_from_target.comment.as_ref().unwrap();
        assert_eq!(comment_target.user.id, target_user_id);
        assert!(matches!(
            command::parse_comment(&comment_target.body),
            command::ParseResult::Command(_)
        ));

        let comment_unauthorized = payload_from_unauthorized.comment.as_ref().unwrap();
        assert_eq!(comment_unauthorized.user.id, unauthorized_user_id);
        assert!(matches!(
            command::parse_comment(&comment_unauthorized.body),
            command::ParseResult::Command(_)
        ));

        // The actual authorization check happens in the webhook handler:
        // comment.user.id == state.target_user_id
        // This test verifies we have the data structures to perform that check.
        assert_ne!(
            comment_target.user.id, comment_unauthorized.user.id,
            "User IDs should be different for authorization testing"
        );
    }

    #[test]
    fn test_webhook_payload_deserialization() {
        // Test that we can deserialize a comment webhook payload correctly
        let json_payload = json!({
            "action": "created",
            "comment": {
                "id": 123,
                "body": "@smaug123-robocop review",
                "user": {
                    "id": 456,
                    "login": "test-user"
                }
            },
            "issue": {
                "number": 789,
                "pull_request": {
                    "url": "https://api.github.com/repos/owner/repo/pulls/789"
                }
            },
            "repository": {
                "name": "repo",
                "full_name": "owner/repo",
                "owner": {
                    "id": 111,
                    "login": "owner"
                }
            },
            "sender": {
                "id": 456,
                "login": "test-user"
            },
            "installation": {
                "id": 999
            }
        });

        let payload: Result<GitHubWebhookPayload, _> = serde_json::from_value(json_payload);
        assert!(payload.is_ok());

        let payload = payload.unwrap();
        assert_eq!(payload.action, Some("created".to_string()));
        assert!(payload.comment.is_some());
        assert!(payload.issue.is_some());

        let comment = payload.comment.unwrap();
        assert_eq!(comment.body, "@smaug123-robocop review");
        assert_eq!(comment.user.id, 456);

        let sender = payload.sender.unwrap();
        assert_eq!(sender.id, 456);
    }

    #[test]
    fn test_build_skipped_check_run_request() {
        // This test verifies that when reviews are disabled for a PR,
        // we create a check run with the correct "skipped" status.
        //
        // This is important because GitHub's Checks API allows PRs to show
        // a clear "skipped" status rather than having no check at all,
        // which provides better visibility into why no review was performed.

        let completed_at = "2024-01-15T10:30:00Z";
        let request = build_skipped_check_run_request(
            12345,          // installation_id
            "test-owner",   // repo_owner
            "test-repo",    // repo_name
            "abc123def456", // head_sha
            completed_at,
        );

        // Verify the check run is marked as completed with skipped conclusion
        assert_eq!(request.status, Some(CheckRunStatus::Completed));
        assert_eq!(request.conclusion, Some(CheckRunConclusion::Skipped));
        assert_eq!(request.completed_at, Some(completed_at));

        // Verify the check run name matches the constant
        assert_eq!(request.name, CHECK_RUN_NAME);

        // Verify the output contains the expected title and summary
        let output = request.output.expect("output should be present");
        assert_eq!(output.title, "Reviews disabled for this PR");
        assert!(
            output.summary.contains("abc123d"),
            "summary should contain truncated SHA"
        );
        assert!(
            output.summary.contains("skipped"),
            "summary should mention 'skipped'"
        );

        // Verify identifiers are passed through correctly
        assert_eq!(request.installation_id, 12345);
        assert_eq!(request.repo_owner, "test-owner");
        assert_eq!(request.repo_name, "test-repo");
        assert_eq!(request.head_sha, "abc123def456");
    }
}
