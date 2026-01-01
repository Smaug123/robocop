//! Effect interpreter that executes effects against real APIs.
//!
//! The interpreter is the boundary between the pure state machine and the
//! impure world of I/O. It takes effects (descriptions of what to do) and
//! executes them, returning result events.

use std::sync::Arc;

use tracing::{error, info, warn};

use super::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use super::event::{DataFetchFailure, Event, FileContent};
use super::state::{BatchId, CancellationReason, CheckRunId, CommentId, CommitSha, ReviewOptions};
use crate::github::{
    CheckRunOutput, CheckRunStatus, CreateCheckRunRequest, FileContentRequest, FileSizeLimits,
    GitHubClient, PullRequestInfo, UpdateCheckRunRequest,
};
use crate::openai::OpenAIClient;
use crate::{get_bot_version, CHECK_RUN_NAME};

/// Context needed by the interpreter to execute effects.
pub struct InterpreterContext {
    pub github_client: Arc<GitHubClient>,
    pub openai_client: Arc<OpenAIClient>,
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub pr_url: Option<String>,
    pub branch_name: Option<String>,
    /// Correlation ID for request tracing.
    pub correlation_id: Option<String>,
}

/// Result of executing an effect.
#[derive(Debug)]
pub enum EffectResult {
    /// Effect completed, produced result events.
    Ok(Vec<Event>),
    /// Effect failed with an error.
    Err(String),
}

impl EffectResult {
    pub fn ok(events: Vec<Event>) -> Self {
        Self::Ok(events)
    }

    pub fn single(event: Event) -> Self {
        Self::Ok(vec![event])
    }

    pub fn none() -> Self {
        Self::Ok(vec![])
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self::Err(msg.into())
    }
}

/// Execute a list of effects and collect result events.
///
/// Effects are executed sequentially. If an effect fails, execution continues
/// with remaining effects, and the error is logged.
pub async fn execute_effects(ctx: &InterpreterContext, effects: Vec<Effect>) -> Vec<Event> {
    let mut result_events = Vec::new();

    for effect in effects {
        match execute_effect(ctx, effect).await {
            EffectResult::Ok(events) => result_events.extend(events),
            EffectResult::Err(err) => {
                error!("Effect execution failed: {}", err);
            }
        }
    }

    result_events
}

/// Execute a single effect.
async fn execute_effect(ctx: &InterpreterContext, effect: Effect) -> EffectResult {
    match effect {
        Effect::FetchData { head_sha, base_sha } => {
            execute_fetch_data(ctx, &head_sha, &base_sha).await
        }

        Effect::CheckAncestry { old_sha, new_sha } => {
            execute_check_ancestry(ctx, &old_sha, &new_sha).await
        }

        Effect::UpdateComment { content } => execute_update_comment(ctx, &content).await,

        Effect::CreateCheckRun {
            head_sha,
            status,
            conclusion,
            title,
            summary,
        } => execute_create_check_run(ctx, &head_sha, status, &title, &summary, conclusion).await,

        Effect::UpdateCheckRun {
            check_run_id,
            status,
            conclusion,
            title,
            summary,
            external_id,
        } => {
            execute_update_check_run(
                ctx,
                check_run_id,
                status,
                conclusion,
                &title,
                &summary,
                external_id.as_ref(),
            )
            .await
        }

        Effect::SubmitBatch {
            diff,
            file_contents,
            head_sha,
            base_sha,
            options,
            reconciliation_token,
        } => {
            execute_submit_batch(
                ctx,
                &diff,
                file_contents,
                &head_sha,
                &base_sha,
                &options,
                &reconciliation_token,
            )
            .await
        }

        Effect::CancelBatch { batch_id } => execute_cancel_batch(ctx, &batch_id).await,

        Effect::Log { level, message } => {
            match level {
                LogLevel::Debug => tracing::debug!("{}", message),
                LogLevel::Info => info!("{}", message),
                LogLevel::Warn => warn!("{}", message),
                LogLevel::Error => error!("{}", message),
            }
            EffectResult::none()
        }
    }
}

/// Fetch diff and file contents from GitHub.
async fn execute_fetch_data(
    ctx: &InterpreterContext,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
) -> EffectResult {
    info!(
        "Fetching data for {}...{}",
        base_sha.short(),
        head_sha.short()
    );

    let correlation_id = ctx.correlation_id.as_deref();

    // Fetch the diff
    let diff = match ctx
        .github_client
        .get_diff(
            correlation_id,
            ctx.installation_id,
            &ctx.repo_owner,
            &ctx.repo_name,
            &base_sha.0,
            &head_sha.0,
        )
        .await
    {
        Ok(diff) => diff,
        Err(e) => {
            return EffectResult::single(Event::DataFetchFailed {
                reason: DataFetchFailure::FetchError {
                    error: e.to_string(),
                },
            });
        }
    };

    if diff.is_empty() {
        return EffectResult::single(Event::DataFetchFailed {
            reason: DataFetchFailure::EmptyDiff,
        });
    }

    // Get changed files from diff
    let changed_files = match ctx
        .github_client
        .get_changed_files_from_diff(
            correlation_id,
            ctx.installation_id,
            &ctx.repo_owner,
            &ctx.repo_name,
            &base_sha.0,
            &head_sha.0,
        )
        .await
    {
        Ok(files) => files,
        Err(e) => {
            return EffectResult::single(Event::DataFetchFailed {
                reason: DataFetchFailure::FetchError {
                    error: e.to_string(),
                },
            });
        }
    };

    if changed_files.is_empty() {
        return EffectResult::single(Event::DataFetchFailed {
            reason: DataFetchFailure::NoFiles,
        });
    }

    // Fetch file contents with limits
    let request = FileContentRequest {
        installation_id: ctx.installation_id,
        repo_owner: ctx.repo_owner.clone(),
        repo_name: ctx.repo_name.clone(),
        sha: head_sha.0.clone(),
        file_paths: changed_files.clone(),
    };

    let limits = FileSizeLimits {
        max_file_size: 100_000,    // 100KB
        max_total_size: 1_000_000, // 1MB
    };

    let (file_contents_map, skipped) = match ctx
        .github_client
        .get_multiple_file_contents_with_limits(correlation_id, &request, &limits)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            return EffectResult::single(Event::DataFetchFailed {
                reason: DataFetchFailure::FetchError {
                    error: e.to_string(),
                },
            });
        }
    };

    // Check if all files were skipped due to size limits
    if !skipped.is_empty() && file_contents_map.is_empty() {
        return EffectResult::single(Event::DataFetchFailed {
            reason: DataFetchFailure::TooLarge {
                skipped_files: skipped,
                total_files: changed_files.len(),
            },
        });
    }

    // Check if there are no files to review at all
    // (e.g., empty diff or all files were binary/empty)
    if file_contents_map.is_empty() && skipped.is_empty() {
        return EffectResult::single(Event::DataFetchFailed {
            reason: DataFetchFailure::NoFiles,
        });
    }

    let file_contents: Vec<FileContent> = file_contents_map
        .into_iter()
        .map(|(path, content)| FileContent { path, content })
        .collect();

    EffectResult::single(Event::DataFetched {
        diff,
        file_contents,
    })
}

/// Check if old_sha is an ancestor of new_sha.
async fn execute_check_ancestry(
    ctx: &InterpreterContext,
    old_sha: &CommitSha,
    new_sha: &CommitSha,
) -> EffectResult {
    info!(
        "Checking ancestry: {} vs {}",
        old_sha.short(),
        new_sha.short()
    );

    let correlation_id = ctx.correlation_id.as_deref();

    let compare_request = crate::github::CompareCommitsRequest {
        installation_id: ctx.installation_id,
        repo_owner: &ctx.repo_owner,
        repo_name: &ctx.repo_name,
        base_sha: &old_sha.0,
        head_sha: &new_sha.0,
    };

    match ctx
        .github_client
        .compare_commits(correlation_id, &compare_request)
        .await
    {
        Ok(comparison) => {
            // old_sha is superseded if it's behind new_sha (new_sha is ahead)
            let is_superseded = comparison.ahead_by > 0 && comparison.behind_by == 0;
            EffectResult::single(Event::AncestryResult {
                old_sha: old_sha.clone(),
                new_sha: new_sha.clone(),
                is_superseded,
            })
        }
        Err(e) => {
            warn!("Failed to check ancestry: {}", e);
            // Emit explicit error event so the state machine can decide what to do
            // (rather than guessing is_superseded and potentially canceling valid batches)
            EffectResult::single(Event::AncestryCheckFailed {
                old_sha: old_sha.clone(),
                new_sha: new_sha.clone(),
                error: e.to_string(),
            })
        }
    }
}

/// Update or create the robocop comment.
///
/// # Known limitation
///
/// If the original comment was deleted and a new one is created, the returned
/// `comment_id` is not propagated back to update the check run's `details_url`.
/// This means the check run may point to a stale URL.
///
/// To fix this properly, we would need to:
/// 1. Return a `CommentUpdated { comment_id }` event from this function
/// 2. Have the state machine track the comment_id and detect changes
/// 3. Emit an UpdateCheckRun effect with the new details_url
///
/// This is a low-priority issue since it only affects the edge case where
/// a user manually deletes the robocop comment.
async fn execute_update_comment(
    ctx: &InterpreterContext,
    content: &CommentContent,
) -> EffectResult {
    let comment_body = format_comment_content(content);
    let correlation_id = ctx.correlation_id.as_deref();
    let version = get_bot_version();

    let pr_info = PullRequestInfo {
        installation_id: ctx.installation_id,
        repo_owner: ctx.repo_owner.clone(),
        repo_name: ctx.repo_name.clone(),
        pr_number: ctx.pr_number,
    };

    match ctx
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &comment_body, &version)
        .await
    {
        Ok(comment_id) => {
            info!("Updated comment {} on PR #{}", comment_id, ctx.pr_number);
            // TODO: Return CommentUpdated event with comment_id to allow
            // updating check run's details_url if the comment was recreated.
            EffectResult::none()
        }
        Err(e) => EffectResult::err(format!("Failed to update comment: {}", e)),
    }
}

/// Format comment content for GitHub.
fn format_comment_content(content: &CommentContent) -> String {
    let version = get_bot_version();

    match content {
        CommentContent::InProgress {
            head_sha,
            batch_id,
            model,
            reasoning_effort,
        } => {
            format!(
                "🤖 **Code review in progress...**\n\n\
                Analyzing commit `{}` using `{}` (reasoning: {}).\n\n\
                Batch ID: `{}`\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                model,
                reasoning_effort,
                batch_id,
                version
            )
        }

        CommentContent::ReviewComplete {
            head_sha,
            batch_id,
            result,
        } => {
            let icon = if result.substantive_comments {
                "🤖"
            } else {
                "✅"
            };

            format!(
                "{} **Code Review Complete**\n\n\
                Commit: `{}`\n\n\
                ## Summary\n{}\n\n\
                <details>\n<summary>Reasoning</summary>\n\n{}\n</details>\n\n\
                ---\n_Robocop v{} | Batch: `{}`_",
                icon,
                head_sha.short(),
                result.summary,
                result.reasoning,
                version,
                batch_id
            )
        }

        CommentContent::ReviewFailed {
            head_sha,
            batch_id,
            reason,
        } => {
            format!(
                "❌ **Code review failed**\n\n\
                Commit: `{}`\n\
                Reason: {}\n\n\
                Batch ID: `{}`\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                reason,
                batch_id,
                version
            )
        }

        CommentContent::ReviewCancelled { head_sha, reason } => {
            let reason_str = match reason {
                CancellationReason::UserRequested => "Cancelled by user request.".to_string(),
                CancellationReason::Superseded { new_sha } => {
                    format!("Superseded by commit `{}`.", new_sha.short())
                }
                CancellationReason::ReviewsDisabled => "Reviews were disabled.".to_string(),
                CancellationReason::External => "Batch was cancelled externally.".to_string(),
                CancellationReason::NoChanges => "No changes to review (empty diff).".to_string(),
                CancellationReason::DiffTooLarge => "Diff was too large to review.".to_string(),
                CancellationReason::NoFiles => "No files to review after filtering.".to_string(),
            };

            format!(
                "❌ **Code review cancelled**\n\n\
                Commit: `{}`\n\
                {}\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                reason_str,
                version
            )
        }

        CommentContent::ReviewSuppressed { head_sha } => {
            format!(
                "ℹ️ **Review suppressed**\n\n\
                Commit: `{}`\n\
                Reviews are disabled for this PR.\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                version
            )
        }

        CommentContent::DiffTooLarge {
            head_sha,
            skipped_files,
            total_files,
        } => {
            // Cap the number of files shown to avoid exceeding GitHub comment limits
            const MAX_FILES_SHOWN: usize = 20;
            let skipped_count = skipped_files.len();
            let files_to_show: Vec<_> = skipped_files.iter().take(MAX_FILES_SHOWN).collect();
            let remaining = skipped_count.saturating_sub(MAX_FILES_SHOWN);

            let file_list = files_to_show
                .iter()
                .map(|f| format!("- `{}`", f))
                .collect::<Vec<_>>()
                .join("\n");

            let file_list_with_overflow = if remaining > 0 {
                format!("{}\n- _...and {} more_", file_list, remaining)
            } else {
                file_list
            };

            format!(
                "⚠️ **Diff too large**\n\n\
                Commit: `{}`\n\
                Skipped {} of {} files due to size limits.\n\n\
                Skipped files:\n{}\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                skipped_count,
                total_files,
                file_list_with_overflow,
                version
            )
        }

        CommentContent::ReviewsEnabled { head_sha } => {
            format!(
                "✅ **Reviews enabled**\n\n\
                Automatic code reviews are now enabled for this PR.\n\
                Starting review for commit `{}`...\n\n\
                ---\n_Robocop v{}_",
                head_sha.short(),
                version
            )
        }

        CommentContent::ReviewsDisabled { cancelled_count } => {
            let cancel_msg = if *cancelled_count > 0 {
                format!("\nCancelled {} pending review(s).", cancelled_count)
            } else {
                String::new()
            };

            format!(
                "🔕 **Reviews disabled**\n\n\
                Automatic code reviews are now disabled for this PR.{}\n\n\
                ---\n_Robocop v{}_",
                cancel_msg, version
            )
        }

        CommentContent::NoReviewsToCancel => {
            format!(
                "ℹ️ No pending reviews to cancel.\n\n\
                ---\n_Robocop v{}_",
                version
            )
        }

        CommentContent::UnrecognizedCommand { attempted } => {
            format!(
                "❓ **Unrecognized command**: `{}`\n\n\
                Available commands:\n\
                - `@smaug123-robocop review` - Request a code review\n\
                - `@smaug123-robocop cancel` - Cancel pending reviews\n\
                - `@smaug123-robocop enable-reviews` - Enable automatic reviews\n\
                - `@smaug123-robocop disable-reviews` - Disable automatic reviews\n\n\
                ---\n_Robocop v{}_",
                attempted, version
            )
        }
    }
}

/// Create a check run.
async fn execute_create_check_run(
    ctx: &InterpreterContext,
    head_sha: &CommitSha,
    status: EffectCheckRunStatus,
    title: &str,
    summary: &str,
    conclusion: Option<EffectCheckRunConclusion>,
) -> EffectResult {
    let github_status = status.to_github();
    let github_conclusion = conclusion.map(|c| c.to_github());

    let now = chrono::Utc::now().to_rfc3339();
    let started_at = if matches!(status, EffectCheckRunStatus::InProgress) {
        Some(now.as_str())
    } else {
        None
    };
    let completed_at = if matches!(status, EffectCheckRunStatus::Completed) {
        Some(now.as_str())
    } else {
        None
    };

    let correlation_id = ctx.correlation_id.as_deref();

    let request = CreateCheckRunRequest {
        installation_id: ctx.installation_id,
        repo_owner: &ctx.repo_owner,
        repo_name: &ctx.repo_name,
        name: CHECK_RUN_NAME,
        head_sha: &head_sha.0,
        details_url: None,
        external_id: None,
        status: Some(github_status),
        started_at,
        conclusion: github_conclusion,
        completed_at,
        output: Some(CheckRunOutput {
            title: title.to_string(),
            summary: summary.to_string(),
            text: None,
        }),
    };

    match ctx
        .github_client
        .create_check_run(correlation_id, &request)
        .await
    {
        Ok(response) => {
            info!("Created check run {} for {}", response.id, head_sha.short());
            EffectResult::none()
        }
        Err(e) => EffectResult::err(format!("Failed to create check run: {}", e)),
    }
}

/// Update an existing check run.
async fn execute_update_check_run(
    ctx: &InterpreterContext,
    check_run_id: CheckRunId,
    status: EffectCheckRunStatus,
    conclusion: Option<EffectCheckRunConclusion>,
    title: &str,
    summary: &str,
    external_id: Option<&BatchId>,
) -> EffectResult {
    let github_status = status.to_github();
    let github_conclusion = conclusion.map(|c| c.to_github());
    let correlation_id = ctx.correlation_id.as_deref();

    let now = chrono::Utc::now().to_rfc3339();
    let started_at = if matches!(status, EffectCheckRunStatus::InProgress) {
        Some(now.as_str())
    } else {
        None
    };
    let completed_at = if matches!(status, EffectCheckRunStatus::Completed) {
        Some(now.as_str())
    } else {
        None
    };

    let external_id_str = external_id.map(|id| id.0.clone());
    let request = UpdateCheckRunRequest {
        installation_id: ctx.installation_id,
        repo_owner: &ctx.repo_owner,
        repo_name: &ctx.repo_name,
        check_run_id: check_run_id.0,
        name: Some(CHECK_RUN_NAME),
        details_url: None,
        external_id: external_id_str.as_deref(),
        status: Some(github_status),
        started_at,
        conclusion: github_conclusion,
        completed_at,
        output: Some(CheckRunOutput {
            title: title.to_string(),
            summary: summary.to_string(),
            text: None,
        }),
    };

    match ctx
        .github_client
        .update_check_run(correlation_id, &request)
        .await
    {
        Ok(_) => {
            info!("Updated check run {}", check_run_id.0);
            EffectResult::none()
        }
        Err(e) => EffectResult::err(format!("Failed to update check run: {}", e)),
    }
}

/// Submit a batch to OpenAI.
async fn execute_submit_batch(
    ctx: &InterpreterContext,
    diff: &str,
    file_contents: Vec<(String, String)>,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
    options: &ReviewOptions,
    reconciliation_token: &str,
) -> EffectResult {
    use robocop_core::ReviewMetadata;

    let correlation_id = ctx.correlation_id.as_deref();
    let version = get_bot_version();

    let model = options
        .model
        .clone()
        .unwrap_or_else(|| crate::openai::DEFAULT_MODEL.to_string());
    let reasoning_effort = options
        .reasoning_effort
        .clone()
        .unwrap_or_else(|| "xhigh".to_string());

    let metadata = ReviewMetadata {
        head_hash: head_sha.0.clone(),
        merge_base: base_sha.0.clone(),
        branch_name: ctx.branch_name.clone(),
        repo_name: ctx.repo_name.clone(),
        remote_url: None,
        pull_request_url: ctx.pr_url.clone(),
    };

    // First, create the comment
    let comment_body = format!(
        "🤖 **Code review in progress...**\n\n\
        Analyzing commit `{}` using `{}` (reasoning: {}).\n\n\
        ---\n_Robocop v{}_",
        head_sha.short(),
        model,
        reasoning_effort,
        version
    );

    let pr_info = PullRequestInfo {
        installation_id: ctx.installation_id,
        repo_owner: ctx.repo_owner.clone(),
        repo_name: ctx.repo_name.clone(),
        pr_number: ctx.pr_number,
    };

    // Create comment (best-effort: continue with OpenAI submission even if this fails)
    let comment_id = match ctx
        .github_client
        .manage_robocop_comment(correlation_id, &pr_info, &comment_body, &version)
        .await
    {
        Ok(id) => Some(CommentId(id)),
        Err(e) => {
            // Log warning but continue - don't fail the review just because GitHub comments failed
            warn!(
                "Failed to create comment for PR #{}: {} - continuing with review",
                ctx.pr_number, e
            );
            None
        }
    };

    // Create check run (best-effort: continue with OpenAI submission even if this fails)
    // Link to the comment so users can click through from GitHub checks
    let details_url = comment_id.and_then(|cid| {
        ctx.pr_url
            .as_ref()
            .map(|pr_url| format!("{}#issuecomment-{}", pr_url, cid.0))
    });
    let now = chrono::Utc::now().to_rfc3339();
    let check_run_request = CreateCheckRunRequest {
        installation_id: ctx.installation_id,
        repo_owner: &ctx.repo_owner,
        repo_name: &ctx.repo_name,
        name: CHECK_RUN_NAME,
        head_sha: &head_sha.0,
        details_url: details_url.as_deref(),
        external_id: None, // batch_id not known yet; set after submission if needed
        status: Some(CheckRunStatus::InProgress),
        started_at: Some(&now),
        conclusion: None,
        completed_at: None,
        output: Some(CheckRunOutput {
            title: "Code review in progress".to_string(),
            summary: format!("Reviewing commit {} with {}", head_sha.short(), model),
            text: None,
        }),
    };

    let check_run_id = match ctx
        .github_client
        .create_check_run(correlation_id, &check_run_request)
        .await
    {
        Ok(response) => Some(CheckRunId(response.id)),
        Err(e) => {
            // Log warning but continue - don't fail the review just because GitHub checks failed
            warn!(
                "Failed to create check run for PR #{}: {} - continuing with review",
                ctx.pr_number, e
            );
            None
        }
    };

    // Submit batch to OpenAI
    match ctx
        .openai_client
        .process_code_review_batch(
            correlation_id,
            diff,
            &file_contents,
            &metadata,
            &reasoning_effort,
            Some(&version),
            None, // additional_prompt
            Some(&model),
            Some(reconciliation_token),
        )
        .await
    {
        Ok(batch_id) => {
            info!("Submitted batch {} for PR #{}", batch_id, ctx.pr_number);
            EffectResult::single(Event::BatchSubmitted {
                batch_id: BatchId(batch_id),
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            })
        }
        Err(e) => EffectResult::single(Event::BatchSubmissionFailed {
            error: e.to_string(),
            comment_id,
            check_run_id,
        }),
    }
}

/// Cancel a batch.
async fn execute_cancel_batch(ctx: &InterpreterContext, batch_id: &BatchId) -> EffectResult {
    info!("Cancelling batch {}", batch_id);

    let correlation_id = ctx.correlation_id.as_deref();

    match ctx
        .openai_client
        .cancel_batch(correlation_id, &batch_id.0)
        .await
    {
        Ok(_) => {
            info!("Cancelled batch {}", batch_id);
            EffectResult::none()
        }
        Err(e) => {
            // Log but don't fail - batch may already be cancelled
            warn!("Failed to cancel batch {}: {}", batch_id, e);
            EffectResult::none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::ReviewResult;

    #[test]
    fn test_format_comment_in_progress() {
        let content = CommentContent::InProgress {
            head_sha: CommitSha::from("abc123def456"),
            batch_id: BatchId::from("batch_123".to_string()),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let formatted = format_comment_content(&content);
        assert!(formatted.contains("Code review in progress"));
        assert!(formatted.contains("abc123d"));
        assert!(formatted.contains("gpt-4"));
        assert!(formatted.contains("batch_123"));
    }

    #[test]
    fn test_format_comment_complete_no_issues() {
        let content = CommentContent::ReviewComplete {
            head_sha: CommitSha::from("abc123"),
            batch_id: BatchId::from("batch_456".to_string()),
            result: ReviewResult {
                reasoning: "Code looks good".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
        };

        let formatted = format_comment_content(&content);
        assert!(formatted.contains("✅"));
        assert!(formatted.contains("Code Review Complete"));
        assert!(formatted.contains("LGTM"));
    }

    #[test]
    fn test_format_comment_cancelled() {
        let content = CommentContent::ReviewCancelled {
            head_sha: CommitSha::from("abc123"),
            reason: CancellationReason::Superseded {
                new_sha: CommitSha::from("def456"),
            },
        };

        let formatted = format_comment_content(&content);
        assert!(formatted.contains("cancelled"));
        assert!(formatted.contains("Superseded"));
        assert!(formatted.contains("def456"));
    }

    /// Regression test: Diff-too-large comment should cap the number of files shown
    /// to avoid exceeding GitHub comment limits.
    #[test]
    fn test_format_comment_diff_too_large_caps_file_list() {
        // Create more than 20 skipped files
        let skipped_files: Vec<String> = (0..30).map(|i| format!("src/file{}.rs", i)).collect();

        let content = CommentContent::DiffTooLarge {
            head_sha: CommitSha::from("abc123"),
            skipped_files,
            total_files: 50,
        };

        let formatted = format_comment_content(&content);

        // Should show first 20 files
        assert!(formatted.contains("src/file0.rs"));
        assert!(formatted.contains("src/file19.rs"));

        // Should NOT show file 20 and beyond (they're in the overflow)
        assert!(!formatted.contains("`src/file20.rs`"));
        assert!(!formatted.contains("`src/file29.rs`"));

        // Should indicate remaining files
        assert!(
            formatted.contains("...and 10 more"),
            "Should show remaining count"
        );

        // Should show correct counts
        assert!(formatted.contains("Skipped 30 of 50 files"));
    }

    /// Test that diff-too-large with few files doesn't show overflow message
    #[test]
    fn test_format_comment_diff_too_large_no_overflow() {
        let skipped_files: Vec<String> = (0..5).map(|i| format!("src/file{}.rs", i)).collect();

        let content = CommentContent::DiffTooLarge {
            head_sha: CommitSha::from("abc123"),
            skipped_files,
            total_files: 10,
        };

        let formatted = format_comment_content(&content);

        // Should show all 5 files
        assert!(formatted.contains("src/file0.rs"));
        assert!(formatted.contains("src/file4.rs"));

        // Should NOT show overflow message
        assert!(
            !formatted.contains("...and"),
            "Should not show overflow for small list"
        );
    }
}
