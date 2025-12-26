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
use crate::github::PullRequestInfo;
use crate::state_machine::{
    cancel_requested_event, disable_reviews_event, enable_reviews_event, pr_updated_event,
    review_requested_event, InterpreterContext, StateMachinePrId,
};
use crate::AppState;
use crate::{CorrelationId, Direction, EventType, RecordedEvent, Sanitizer};

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

/// Cancel pending reviews using the state machine.
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

    // Create state machine PR ID
    let sm_pr_id = StateMachinePrId::new(repo_owner, repo_name, pr_number);

    // Create the cancel event
    let event = cancel_requested_event();

    // Create interpreter context
    let pr_url = format!("https://github.com/{}/pull/{}", repo.full_name, pr_number);
    let ctx = InterpreterContext {
        github_client: state.github_client.clone(),
        openai_client: state.openai_client.clone(),
        installation_id,
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
        pr_url: Some(pr_url),
        branch_name: None,
        correlation_id: correlation_id.map(|s| s.to_string()),
    };

    // Process the event through the state machine
    let final_state = state
        .state_store
        .process_event(&sm_pr_id, event, &ctx)
        .await;

    info!(
        "Cancel reviews completed for PR #{}, final state: {:?}",
        pr_number, final_state
    );

    Ok(())
}

/// Enable reviews for a PR using the state machine.
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

    // Extract review options from PR body if present
    let review_options = pr_details
        .body
        .as_ref()
        .and_then(|body| command::extract_review_options(body));

    if review_options.is_some() {
        info!(
            "Found review options in PR description: {:?}",
            review_options
        );
    }

    // Create state machine PR ID
    let sm_pr_id = StateMachinePrId::new(repo_owner, repo_name, pr_number);

    // Create enable reviews event with current SHAs and options from PR body
    let event = enable_reviews_event(&pr_details.head.sha, &pr_details.base.sha, review_options);

    // Create interpreter context
    let pr_url = format!("https://github.com/{}/pull/{}", repo.full_name, pr_number);
    let ctx = InterpreterContext {
        github_client: state.github_client.clone(),
        openai_client: state.openai_client.clone(),
        installation_id,
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
        pr_url: Some(pr_url),
        branch_name: Some(pr_details.head.ref_name.clone()),
        correlation_id: correlation_id.map(|s| s.to_string()),
    };

    // Process the event through the state machine
    let final_state = state
        .state_store
        .process_event(&sm_pr_id, event, &ctx)
        .await;

    // Update the cached review state to reflect the enable command
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };
    state
        .review_states
        .write()
        .await
        .insert(pr_id, crate::ReviewState::Enabled);

    info!(
        "Enable reviews completed for PR #{}, final state: {:?}",
        pr_number, final_state
    );

    Ok(())
}

/// Disable reviews for a PR using the state machine.
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

    // Create state machine PR ID
    let sm_pr_id = StateMachinePrId::new(repo_owner, repo_name, pr_number);

    // Create disable reviews event
    let event = disable_reviews_event();

    // Create interpreter context
    let pr_url = format!("https://github.com/{}/pull/{}", repo.full_name, pr_number);
    let ctx = InterpreterContext {
        github_client: state.github_client.clone(),
        openai_client: state.openai_client.clone(),
        installation_id,
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
        pr_url: Some(pr_url),
        branch_name: None,
        correlation_id: correlation_id.map(|s| s.to_string()),
    };

    // Process the event through the state machine
    let final_state = state
        .state_store
        .process_event(&sm_pr_id, event, &ctx)
        .await;

    // Update the cached review state to reflect the disable command
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };
    state
        .review_states
        .write()
        .await
        .insert(pr_id, crate::ReviewState::Disabled);

    info!(
        "Disable reviews completed for PR #{}, final state: {:?}",
        pr_number, final_state
    );

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

    // Use the state machine code review logic with force flag and review options
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

/// Process a code review using the state machine.
///
/// This implementation uses an explicit state machine for managing review lifecycle.
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
        "Processing code review (v2) for PR #{} in {} (force: {}, options: {:?})",
        pr.number, repo.full_name, force_review, review_options
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number: pr.number,
    };

    // Get or rehydrate review state to determine if reviews are enabled
    let reviews_enabled = {
        let states = state.review_states.read().await;
        if let Some(&cached_state) = states.get(&pr_id) {
            cached_state == crate::ReviewState::Enabled
        } else {
            drop(states); // Release read lock before expensive operation

            // Rehydrate state from PR history
            let rehydrated_state = crate::review_state::rehydrate_review_state(
                &state.github_client,
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
            rehydrated_state == crate::ReviewState::Enabled
        }
    };

    // Create state machine PR ID
    let sm_pr_id = StateMachinePrId::new(repo_owner, repo_name, pr.number);

    // Initialize or get existing state from state store
    let _current_state = state
        .state_store
        .get_or_init(&sm_pr_id, reviews_enabled)
        .await;

    // Create the appropriate event
    let event = if force_review {
        review_requested_event(&pr.head.sha, &pr.base.sha, review_options)
    } else {
        pr_updated_event(
            &pr.head.sha,
            &pr.base.sha,
            false, // force_review
            review_options,
        )
    };

    // Create interpreter context
    let pr_url = format!("https://github.com/{}/pull/{}", repo.full_name, pr.number);
    let ctx = InterpreterContext {
        github_client: state.github_client.clone(),
        openai_client: state.openai_client.clone(),
        installation_id,
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number: pr.number,
        pr_url: Some(pr_url),
        branch_name: Some(pr.head.ref_name.clone()),
        correlation_id: correlation_id.map(|s| s.to_string()),
    };

    // Process the event through the state machine
    let final_state = state
        .state_store
        .process_event(&sm_pr_id, event, &ctx)
        .await;

    info!(
        "Code review completed for PR #{}, final state: {:?}",
        pr.number, final_state
    );

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
}
