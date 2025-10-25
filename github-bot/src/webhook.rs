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
use crate::github::{FileContentRequest, FileSizeLimits, PullRequestInfo};
use crate::openai::ReviewMetadata;
use crate::recording::sanitizer::Sanitizer;
use crate::recording::types::CorrelationId;
use crate::recording::{Direction, EventType, RecordedEvent};
use crate::AppState;

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
        Some("opened") | Some("synchronize") => {
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
                    if let Some(robocop_command) = command::parse_comment(&comment.body) {
                        info!("Found robocop command: {}", robocop_command);

                        match robocop_command {
                            command::RobocopCommand::Review => {
                                info!("Processing @robocop review command");

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
                                        )
                                        .await
                                        {
                                            error!("Failed to process manual review: {}", e);
                                        }
                                    });
                                } else {
                                    warn!("Missing repository or installation info for review command");
                                }
                            }
                            command::RobocopCommand::Cancel => {
                                info!("Processing @robocop cancel command");

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

    for (batch_id, _batch) in &batches_to_cancel {
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
                        }
                    }
                    Err(e) => {
                        error!("Failed to check status of batch {}: {}", batch_id, e);
                    }
                }
            }
        }
    }

    // Remove all batches from tracking
    {
        let mut pending = state.pending_batches.write().await;
        for (batch_id, _) in &batches_to_cancel {
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

async fn process_manual_review(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Processing manual review request for PR #{} in {}",
        pr_number, repo.full_name
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
    };

    // Use the existing code review logic
    process_code_review(correlation_id, state, installation_id, repo, pr).await
}

async fn process_code_review(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr: PullRequest,
) -> anyhow::Result<()> {
    info!(
        "Processing code review for PR #{} in {}",
        pr.number, repo.full_name
    );

    let github_client = &state.github_client;
    let openai_client = &state.openai_client;

    // Extract repository info
    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let base_sha = &pr.base.sha;
    let head_sha = &pr.head.sha;
    let branch_name = Some(pr.head.ref_name.as_str());

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

    // Get file contents with size limits (50KB per file, 1MB total)
    let limits = FileSizeLimits {
        max_file_size: 50_000,     // 50KB
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
            **Limits:** Max 50KB per file, 1MB total\n\n\
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

    let batch_id = openai_client
        .process_code_review_batch(
            correlation_id,
            &diff,
            &file_contents,
            &metadata,
            "high", // reasoning_effort - could be configurable
        )
        .await?;

    info!(
        "Successfully submitted batch request {} for PR #{} in {}",
        batch_id, pr.number, repo.full_name
    );

    // Create or update PR comment to show review is in progress
    let version = crate::get_bot_version();
    let in_progress_content = format!(
        "🤖 **Code review in progress...**\n\n\
        I'm analyzing the changes in this pull request. This may take a long time depending on current OpenAI load.\n\n\
        **Commit:** `{}`\n\
        **Batch ID:** `{}`\n\
        **Status:** Processing",
        head_sha, batch_id
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

    // Store batch for polling
    let pending_batch = crate::PendingBatch {
        batch_id: batch_id.clone(),
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number: pr.number,
        comment_id,
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
        let is_ancestor_result = {
            let github_client = &state.github_client;
            GitOps::is_ancestor(
                github_client,
                installation_id,
                repo_owner,
                repo_name,
                new_head_sha,
                &batch.head_sha,
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
    use crate::github::GitHubClient;
    use crate::openai::OpenAIClient;
    use serde_json::json;

    fn create_test_state(target_user_id: u64) -> Arc<AppState> {
        Arc::new(AppState {
            github_client: GitHubClient::new(123456, "test-key".to_string()),
            openai_client: OpenAIClient::new("test-api-key".to_string()),
            webhook_secret: "test-secret".to_string(),
            target_user_id,
            pending_batches: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            recording_logger: None,
        })
    }

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
                    url: format!("https://api.github.com/repos/test-owner/test-repo/pulls/{}", pr_number),
                }),
            }),
        }
    }

    #[test]
    fn test_comment_events_check_user_authorization() {
        // This test verifies that the webhook handler code checks user authorization
        // for comment events, just like it does for PR events.
        //
        // SECURITY REQUIREMENT: Only the configured target_user_id should be able to
        // trigger @robocop commands via PR comments. This prevents unauthorized users
        // from depleting OpenAI credits.
        //
        // This test inspects the source code to verify the authorization check exists.

        let source_code = include_str!("webhook.rs");

        // Find the comment event handler section (action == "created")
        let comment_section_start = source_code
            .find(r#"Some("created") =>"#)
            .expect("Could not find comment event handler");

        // Find the end of the comment section (next match arm or end of match)
        let after_comment_section = &source_code[comment_section_start..];
        let comment_section_end = after_comment_section
            .find("\n        _ =>")
            .expect("Could not find end of comment event handler");

        let comment_handler_code = &after_comment_section[..comment_section_end];

        // CRITICAL CHECK: The comment handler MUST check if comment.user.id == state.target_user_id
        // just like the PR event handler does (see line 217: "if sender.id == state.target_user_id")

        // Check that we're verifying the user ID before processing commands
        let has_user_check = comment_handler_code.contains("comment.user.id")
            && comment_handler_code.contains("target_user_id");

        assert!(
            has_user_check,
            "SECURITY BUG: Comment event handler does not check if comment.user.id == state.target_user_id!\n\
            This allows ANY GitHub user to trigger @robocop commands and deplete OpenAI credits.\n\n\
            The fix should add a check like:\n\
            if comment.user.id == state.target_user_id {{\n\
                // process commands\n\
            }} else {{\n\
                info!(\"Ignoring command from unauthorized user\");\n\
            }}\n\n\
            Compare with PR event handler at line 217 which correctly checks: sender.id == state.target_user_id"
        );
    }

    #[test]
    fn test_webhook_payload_deserialization() {
        // Test that we can deserialize a comment webhook payload correctly
        let json_payload = json!({
            "action": "created",
            "comment": {
                "id": 123,
                "body": "@robocop review",
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
        assert_eq!(comment.body, "@robocop review");
        assert_eq!(comment.user.id, 456);

        let sender = payload.sender.unwrap();
        assert_eq!(sender.id, 456);
    }
}
