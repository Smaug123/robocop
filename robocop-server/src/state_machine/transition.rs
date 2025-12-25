//! Pure state transition function.
//!
//! The transition function is the core of the state machine. It takes the
//! current state and an event, and returns the new state and a list of effects.
//! This function has NO side effects - it is pure and deterministic.

use super::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use super::event::{DataFetchFailure, Event};
use super::state::{CancellationReason, FailureReason, ReviewMachineState, ReviewResult};

/// GitHub's maximum length for check run titles.
const GITHUB_TITLE_MAX_LEN: usize = 255;

/// Sanitize a check run title to meet GitHub constraints.
///
/// - Replaces newlines with spaces
/// - Truncates to 255 characters (GitHub's limit)
/// - Adds ellipsis if truncated
fn sanitize_check_run_title(title: &str) -> String {
    // Replace newlines with spaces for single-line display
    let single_line = title.replace('\n', " ").replace('\r', "");

    // Truncate if necessary
    if single_line.len() <= GITHUB_TITLE_MAX_LEN {
        single_line
    } else {
        // Leave room for "..."
        let truncate_at = GITHUB_TITLE_MAX_LEN - 3;
        // Find a good break point (don't cut in middle of word if possible)
        let break_point = single_line[..truncate_at].rfind(' ').unwrap_or(truncate_at);
        format!("{}...", &single_line[..break_point])
    }
}

/// Result of a state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionResult {
    /// The new state after the transition.
    pub state: ReviewMachineState,
    /// Effects to execute.
    pub effects: Vec<Effect>,
}

impl TransitionResult {
    pub fn new(state: ReviewMachineState, effects: Vec<Effect>) -> Self {
        Self { state, effects }
    }

    pub fn no_change(state: ReviewMachineState) -> Self {
        Self {
            state,
            effects: vec![],
        }
    }
}

/// Pure state transition function.
///
/// Given the current state and an event, returns the new state and effects to execute.
/// This function has NO side effects - all effects are returned as data.
pub fn transition(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
        // =====================================================================
        // Idle State Transitions
        // =====================================================================

        // PR updated while idle and reviews enabled -> start preparing
        (
            ReviewMachineState::Idle {
                reviews_enabled: true,
            },
            Event::PrUpdated {
                head_sha,
                base_sha,
                options,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // PR updated while idle and reviews disabled -> post suppression notice
        (
            ReviewMachineState::Idle {
                reviews_enabled: false,
            },
            Event::PrUpdated {
                head_sha,
                force_review: false,
                ..
            },
        ) => TransitionResult::new(
            state.clone(),
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewSuppressed {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::CreateCheckRun {
                    head_sha,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled for this PR".to_string(),
                    summary: "Code review was skipped because reviews are disabled.".to_string(),
                },
            ],
        ),

        // Force review requested (even when disabled) -> start preparing
        (
            ReviewMachineState::Idle { reviews_enabled },
            Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // Enable reviews while idle -> set enabled and trigger review
        (
            ReviewMachineState::Idle { .. },
            Event::EnableReviewsRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewsEnabled {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // Disable reviews while idle -> just update flag
        (ReviewMachineState::Idle { .. }, Event::DisableReviewsRequested) => TransitionResult::new(
            ReviewMachineState::Idle {
                reviews_enabled: false,
            },
            vec![Effect::UpdateComment {
                content: CommentContent::ReviewsDisabled { cancelled_count: 0 },
            }],
        ),

        // Cancel requested while idle -> nothing to cancel
        (ReviewMachineState::Idle { .. }, Event::CancelRequested) => TransitionResult::new(
            state.clone(),
            vec![Effect::UpdateComment {
                content: CommentContent::NoReviewsToCancel,
            }],
        ),

        // =====================================================================
        // Preparing State Transitions
        // =====================================================================

        // Data fetched successfully -> submit batch
        (
            ReviewMachineState::Preparing {
                head_sha,
                base_sha,
                options,
                reviews_enabled: _,
            },
            Event::DataFetched {
                diff,
                file_contents,
            },
        ) => {
            let file_contents_tuples: Vec<(String, String)> = file_contents
                .into_iter()
                .map(|fc| (fc.path, fc.content))
                .collect();

            TransitionResult::new(
                state.clone(), // Stay in Preparing until batch is submitted
                vec![Effect::SubmitBatch {
                    diff,
                    file_contents: file_contents_tuples,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    options: options.clone(),
                }],
            )
        }

        // Data fetch failed -> transition to failed/cancelled
        (
            ReviewMachineState::Preparing {
                head_sha,
                reviews_enabled,
                ..
            },
            Event::DataFetchFailed { reason },
        ) => {
            let (new_state, effects) = match reason {
                DataFetchFailure::EmptyDiff => (
                    ReviewMachineState::Cancelled {
                        reviews_enabled: *reviews_enabled,
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::ReviewsDisabled, // No changes to review
                        pending_cancel_batch_id: None,
                    },
                    vec![Effect::Log {
                        level: LogLevel::Info,
                        message: "Empty diff, skipping review".to_string(),
                    }],
                ),
                DataFetchFailure::TooLarge {
                    skipped_files,
                    total_files,
                } => (
                    ReviewMachineState::Cancelled {
                        reviews_enabled: *reviews_enabled,
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::ReviewsDisabled,
                        pending_cancel_batch_id: None,
                    },
                    vec![Effect::UpdateComment {
                        content: CommentContent::DiffTooLarge {
                            head_sha: head_sha.clone(),
                            skipped_files,
                            total_files,
                        },
                    }],
                ),
                DataFetchFailure::FetchError { error } => (
                    ReviewMachineState::Failed {
                        reviews_enabled: *reviews_enabled,
                        head_sha: head_sha.clone(),
                        reason: FailureReason::DataFetchFailed { reason: error },
                    },
                    vec![],
                ),
                DataFetchFailure::NoFiles => (
                    ReviewMachineState::Cancelled {
                        reviews_enabled: *reviews_enabled,
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::ReviewsDisabled,
                        pending_cancel_batch_id: None,
                    },
                    vec![Effect::Log {
                        level: LogLevel::Info,
                        message: "No files to review".to_string(),
                    }],
                ),
            };
            TransitionResult::new(new_state, effects)
        }

        // Batch submitted successfully -> transition to BatchPending
        (
            ReviewMachineState::Preparing {
                head_sha,
                base_sha,
                reviews_enabled,
                ..
            },
            Event::BatchSubmitted {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            },
        ) => {
            let mut effects = vec![Effect::UpdateComment {
                content: CommentContent::InProgress {
                    head_sha: head_sha.clone(),
                    batch_id: batch_id.clone(),
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                },
            }];

            // Update check run with batch ID as external_id for correlation
            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: cr_id,
                    status: EffectCheckRunStatus::InProgress,
                    conclusion: None,
                    title: "Code review in progress".to_string(),
                    summary: format!(
                        "Reviewing commit {} with {} (batch: {})",
                        head_sha.short(),
                        model,
                        batch_id
                    ),
                    external_id: Some(batch_id.clone()),
                });
            }

            TransitionResult::new(
                ReviewMachineState::BatchPending {
                    reviews_enabled: *reviews_enabled,
                    batch_id: batch_id.clone(),
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    comment_id,
                    check_run_id,
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                },
                effects,
            )
        }

        // Batch submission failed -> transition to Failed
        (
            ReviewMachineState::Preparing {
                head_sha,
                reviews_enabled,
                ..
            },
            Event::BatchSubmissionFailed {
                error,
                comment_id: _,
                check_run_id,
            },
        ) => {
            // Build cleanup effects for any UI elements that were created
            let mut effects = vec![
                // Update comment to show failure (manage_robocop_comment will find existing)
                Effect::UpdateComment {
                    content: CommentContent::ReviewFailed {
                        head_sha: head_sha.clone(),
                        batch_id: super::state::BatchId::from("(submission failed)".to_string()),
                        reason: FailureReason::SubmissionFailed {
                            error: error.clone(),
                        },
                    },
                },
            ];

            // Update check run if it was created
            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Failure),
                    title: "Code review failed".to_string(),
                    summary: format!("Batch submission failed: {}", error),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Failed {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    reason: FailureReason::SubmissionFailed { error },
                },
                effects,
            )
        }

        // Cancel while preparing -> go back to idle
        (
            ReviewMachineState::Preparing {
                reviews_enabled, ..
            },
            Event::CancelRequested,
        ) => TransitionResult::new(
            ReviewMachineState::Idle {
                reviews_enabled: *reviews_enabled,
            },
            vec![Effect::UpdateComment {
                content: CommentContent::NoReviewsToCancel,
            }],
        ),

        // Same commit while preparing -> no change (duplicate request)
        (
            ReviewMachineState::Preparing { head_sha, .. },
            Event::ReviewRequested {
                head_sha: new_head_sha,
                ..
            },
        ) if head_sha == &new_head_sha => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Ignoring duplicate ReviewRequested for same commit {}",
                    head_sha.short()
                ),
            }],
        ),

        // ReviewRequested while preparing -> restart with new options
        (
            ReviewMachineState::Preparing {
                reviews_enabled, ..
            },
            Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // Same commit while preparing -> no change (duplicate webhook)
        (
            ReviewMachineState::Preparing { head_sha, .. },
            Event::PrUpdated {
                head_sha: new_head_sha,
                ..
            },
        ) if head_sha == &new_head_sha => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Ignoring duplicate PrUpdated for same commit {}",
                    head_sha.short()
                ),
            }],
        ),

        // PrUpdated while preparing -> restart with new commit
        (
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                ..
            },
            Event::PrUpdated {
                head_sha,
                base_sha,
                options,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // PrUpdated while preparing (reviews disabled) -> show suppression and stay idle
        (
            ReviewMachineState::Preparing {
                reviews_enabled: false,
                ..
            },
            Event::PrUpdated {
                head_sha,
                force_review: false,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::Idle {
                reviews_enabled: false,
            },
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewSuppressed {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::CreateCheckRun {
                    head_sha,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled for this PR".to_string(),
                    summary: "Code review was skipped because reviews are disabled.".to_string(),
                },
            ],
        ),

        // Disable reviews while preparing -> go to Idle with reviews disabled
        (ReviewMachineState::Preparing { .. }, Event::DisableReviewsRequested) => {
            TransitionResult::new(
                ReviewMachineState::Idle {
                    reviews_enabled: false,
                },
                vec![Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 0 },
                }],
            )
        }

        // Enable reviews while preparing (already preparing, so update options)
        (
            ReviewMachineState::Preparing { .. },
            Event::EnableReviewsRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewsEnabled {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // =====================================================================
        // BatchPending State Transitions
        // =====================================================================

        // Batch status update (still processing) -> no state change
        (
            ReviewMachineState::BatchPending { batch_id, .. },
            Event::BatchStatusUpdate {
                batch_id: event_batch_id,
                status,
            },
        ) if batch_id == &event_batch_id && status.is_processing() => {
            TransitionResult::no_change(state.clone())
        }

        // Batch completed -> transition to Completed
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::BatchCompleted {
                batch_id: event_batch_id,
                result,
            },
        ) if batch_id == &event_batch_id => {
            let conclusion = match &result {
                ReviewResult::NoIssues { .. } => EffectCheckRunConclusion::Success,
                ReviewResult::HasIssues { .. } => EffectCheckRunConclusion::Failure,
            };
            let (title, result_summary) = match &result {
                ReviewResult::NoIssues { summary, .. } => {
                    ("Code review passed".to_string(), summary.clone())
                }
                ReviewResult::HasIssues { summary, .. } => (
                    sanitize_check_run_title(&format!("Code review found issues: {}", summary)),
                    summary.clone(),
                ),
            };

            let new_state = ReviewMachineState::Completed {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                result: result.clone(),
            };

            let mut effects = vec![Effect::UpdateComment {
                content: CommentContent::ReviewComplete {
                    head_sha: head_sha.clone(),
                    batch_id: batch_id.clone(),
                    result,
                },
            }];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(conclusion),
                    title,
                    summary: result_summary,
                    external_id: None,
                });
            }

            TransitionResult::new(new_state, effects)
        }

        // Batch cancelled externally (found via polling) -> transition to Cancelled
        // This is different from CancelRequested which is user-initiated
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::BatchTerminated {
                batch_id: event_batch_id,
                reason: FailureReason::BatchCancelled,
            },
        ) if batch_id == &event_batch_id => {
            let mut effects = vec![Effect::UpdateComment {
                content: CommentContent::ReviewCancelled {
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::External, // Batch was cancelled externally
                },
            }];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Cancelled),
                    title: "Review cancelled".to_string(),
                    summary: "The batch was cancelled externally.".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::External,
                    pending_cancel_batch_id: None, // Already done
                },
                effects,
            )
        }

        // Batch terminated (failed/expired) -> transition to Failed
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::BatchTerminated {
                batch_id: event_batch_id,
                reason,
            },
        ) if batch_id == &event_batch_id => {
            let conclusion = match &reason {
                FailureReason::BatchExpired => EffectCheckRunConclusion::TimedOut,
                _ => EffectCheckRunConclusion::Failure,
            };

            let new_state = ReviewMachineState::Failed {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                reason: reason.clone(),
            };

            let mut effects = vec![Effect::UpdateComment {
                content: CommentContent::ReviewFailed {
                    head_sha: head_sha.clone(),
                    batch_id: batch_id.clone(),
                    reason,
                },
            }];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(conclusion),
                    title: "Code review failed".to_string(),
                    summary: "Review failed to complete".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(new_state, effects)
        }

        // Cancel requested while batch pending -> cancel batch
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::CancelRequested,
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::UserRequested,
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Review cancelled by user".to_string(),
                    summary: "The review was cancelled at the user's request.".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::UserRequested,
                    // Track batch for polling in case cancel fails
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // Disable reviews while batch pending -> cancel and disable
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                ..
            },
            Event::DisableReviewsRequested,
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 1 },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled".to_string(),
                    summary: "Reviews were disabled for this PR.".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::ReviewsDisabled,
                    // Track batch for polling in case cancel fails
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // Enable reviews while batch pending -> flip flag and acknowledge
        // The batch keeps running; user just enabled automatic reviews going forward.
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                ..
            },
            Event::EnableReviewsRequested { .. },
        ) => TransitionResult::new(
            ReviewMachineState::BatchPending {
                reviews_enabled: true,
                batch_id: batch_id.clone(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id: *comment_id,
                check_run_id: *check_run_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
            },
            vec![Effect::UpdateComment {
                content: CommentContent::ReviewsEnabled {
                    head_sha: head_sha.clone(),
                },
            }],
        ),

        // ReviewRequested for SAME commit while batch pending -> no-op (de-dup)
        (
            ReviewMachineState::BatchPending {
                head_sha: pending_sha,
                ..
            },
            Event::ReviewRequested {
                head_sha: requested_sha,
                ..
            },
        ) if pending_sha == &requested_sha => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Review already pending for commit {}, ignoring duplicate request",
                    pending_sha.short()
                ),
            }],
        ),

        // ReviewRequested while batch pending -> cancel current and start new
        (
            ReviewMachineState::BatchPending {
                batch_id,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => {
            let mut effects = vec![Effect::CancelBatch {
                batch_id: batch_id.clone(),
            }];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Review restarted".to_string(),
                    summary: "A new review was manually requested.".to_string(),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: *reviews_enabled,
                    head_sha,
                    base_sha,
                    options,
                },
                effects,
            )
        }

        // New commit while batch pending -> check ancestry
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                reviews_enabled,
                model,
                reasoning_effort,
            },
            Event::PrUpdated {
                head_sha: new_head_sha,
                base_sha: new_base_sha,
                options,
                ..
            },
        ) if head_sha != &new_head_sha => TransitionResult::new(
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: *reviews_enabled,
                batch_id: batch_id.clone(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id: *comment_id,
                check_run_id: *check_run_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                new_head_sha: new_head_sha.clone(),
                new_base_sha,
                new_options: options,
            },
            vec![Effect::CheckAncestry {
                old_sha: head_sha.clone(),
                new_sha: new_head_sha,
            }],
        ),

        // Same commit while batch pending -> no change (duplicate webhook)
        (
            ReviewMachineState::BatchPending { head_sha, .. },
            Event::PrUpdated {
                head_sha: new_head_sha,
                ..
            },
        ) if head_sha == &new_head_sha => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Ignoring duplicate PrUpdated for same commit {}",
                    head_sha.short()
                ),
            }],
        ),

        // =====================================================================
        // AwaitingAncestryCheck State Transitions
        // =====================================================================

        // Old commit is superseded and reviews are enabled -> cancel old, start new review
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                reviews_enabled: true,
                new_options,
                ..
            },
            Event::AncestryResult {
                is_superseded: true,
                ..
            },
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was superseded by a newer commit: {}",
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: true,
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                    options: new_options.clone(),
                },
                effects,
            )
        }

        // Old commit is superseded but reviews are disabled -> cancel old, don't start new
        // This handles the case where a forced review was running and a new commit arrived,
        // but reviews are disabled so we shouldn't start a new automatic review.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                reviews_enabled: false,
                ..
            },
            Event::AncestryResult {
                is_superseded: true,
                ..
            },
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Commit {} superseded by {}, cancelling review (reviews disabled, not starting new)",
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was superseded by a newer commit: {} (reviews disabled)",
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: old_sha.clone(),
                    reason: CancellationReason::Superseded {
                        new_sha: new_head_sha.clone(),
                    },
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // Old commit is NOT superseded, but reviews are disabled -> cancel old, don't start new
        // This handles the case where a forced review was running and a force-push occurred,
        // but reviews are disabled so we shouldn't start a new one.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                reviews_enabled: false,
                ..
            },
            Event::AncestryResult {
                is_superseded: false,
                ..
            },
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Commits {} and {} diverged, cancelling old review (reviews disabled, not starting new)",
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Replaced by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was replaced by a newer commit: {} (reviews disabled)",
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: old_sha.clone(),
                    reason: CancellationReason::Superseded {
                        new_sha: new_head_sha.clone(),
                    },
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // Old commit is NOT superseded (e.g., force-push/rebase) -> cancel old, start new review
        // When commits diverge (no ancestor relationship), the old commit is no longer relevant
        // since the branch has been rewritten. Cancel the old batch and review the new commit.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                reviews_enabled,
                new_options,
                ..
            },
            Event::AncestryResult {
                is_superseded: false,
                ..
            },
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Commits {} and {} diverged (force-push/rebase), cancelling old review and starting new",
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Replaced by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was replaced by a newer commit (force-push/rebase): {}",
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: *reviews_enabled,
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                    options: new_options.clone(),
                },
                effects,
            )
        }

        // Ancestry check failed (GitHub API error) -> cancel old, start new review
        // We can't determine the ancestry relationship, but we should prioritize
        // reviewing the latest commit. Cancel the old batch and start a new review.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled,
                batch_id,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::AncestryCheckFailed { error, .. },
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::Log {
                    level: LogLevel::Warn,
                    message: format!(
                        "Ancestry check failed ({}), cancelling batch {} for {} and starting new review for {}",
                        error,
                        batch_id,
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Replaced by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was replaced by a newer commit: {} (ancestry check failed)",
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: *reviews_enabled,
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                    options: new_options.clone(),
                },
                effects,
            )
        }

        // BatchCompleted while awaiting ancestry check (reviews enabled) -> discard old results, start new review
        // The batch completed for the old commit, but a new commit arrived. The old results
        // are stale, so discard them and start reviewing the new commit.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: true,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::BatchCompleted { result, .. },
        ) => {
            let mut effects = vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Batch completed for {} but new commit {} arrived, discarding results and starting new review",
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                // Mark as stale - the results are for an old commit
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "Review completed but superseded by newer commit: {}. Result was: {}",
                        new_head_sha.short(),
                        match &result {
                            ReviewResult::NoIssues { summary, .. } =>
                                format!("No issues - {}", summary),
                            ReviewResult::HasIssues { summary, .. } =>
                                format!("Issues found - {}", summary),
                        }
                    ),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: true,
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                    options: new_options.clone(),
                },
                effects,
            )
        }

        // BatchCompleted while awaiting ancestry check (reviews disabled) -> discard old results, don't start new
        // The forced review completed but a new commit arrived. Since reviews are disabled,
        // we don't start a new review automatically.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: false,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                ..
            },
            Event::BatchCompleted { result, .. },
        ) => {
            let mut effects = vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Batch completed for {} but new commit {} arrived (reviews disabled), discarding results",
                        old_sha.short(),
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "Review completed but superseded by newer commit: {} (reviews disabled). Result was: {}",
                        new_head_sha.short(),
                        match &result {
                            ReviewResult::NoIssues { summary, .. } => format!("No issues - {}", summary),
                            ReviewResult::HasIssues { summary, .. } => format!("Issues found - {}", summary),
                        }
                    ),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: old_sha.clone(),
                    reason: CancellationReason::Superseded {
                        new_sha: new_head_sha.clone(),
                    },
                    pending_cancel_batch_id: None, // Batch already completed
                },
                effects,
            )
        }

        // BatchTerminated while awaiting ancestry check (reviews enabled) -> start new review
        // The batch failed/expired for the old commit, but a new commit arrived anyway.
        // Start reviewing the new commit.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: true,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::BatchTerminated { reason, .. },
        ) => {
            let mut effects = vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Batch for {} terminated ({}) but new commit {} arrived, starting new review",
                        old_sha.short(),
                        reason,
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "Review failed ({}) and superseded by newer commit: {}",
                        reason,
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: true,
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                    options: new_options.clone(),
                },
                effects,
            )
        }

        // BatchTerminated while awaiting ancestry check (reviews disabled) -> don't start new
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: false,
                head_sha: old_sha,
                check_run_id,
                new_head_sha,
                ..
            },
            Event::BatchTerminated { reason, .. },
        ) => {
            let mut effects = vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Batch for {} terminated ({}) and new commit {} arrived (reviews disabled)",
                        old_sha.short(),
                        reason,
                        new_head_sha.short()
                    ),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: old_sha.clone(),
                        reason: CancellationReason::Superseded {
                            new_sha: new_head_sha.clone(),
                        },
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "Review failed ({}) and superseded by newer commit: {} (reviews disabled)",
                        reason,
                        new_head_sha.short()
                    ),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: old_sha.clone(),
                    reason: CancellationReason::Superseded {
                        new_sha: new_head_sha.clone(),
                    },
                    pending_cancel_batch_id: None, // Batch already terminated
                },
                effects,
            )
        }

        // ReviewRequested while awaiting ancestry check -> cancel current and start new
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => {
            let mut effects = vec![Effect::CancelBatch {
                batch_id: batch_id.clone(),
            }];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Review restarted".to_string(),
                    summary: "A new review was manually requested.".to_string(),
                    external_id: None,
                });
            }

            effects.push(Effect::FetchData {
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
            });

            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: *reviews_enabled,
                    head_sha,
                    base_sha,
                    options,
                },
                effects,
            )
        }

        // PrUpdated while awaiting ancestry check -> update to track newest commit
        // When yet another commit arrives while waiting for ancestry, just update
        // the new_* fields to track the latest commit.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled,
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                ..
            },
            Event::PrUpdated {
                head_sha: newest_head_sha,
                base_sha: newest_base_sha,
                options: newest_options,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: *reviews_enabled,
                batch_id: batch_id.clone(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id: *comment_id,
                check_run_id: *check_run_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                new_head_sha: newest_head_sha,
                new_base_sha: newest_base_sha,
                new_options: newest_options,
            },
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Updated pending commit while awaiting ancestry check".to_string(),
            }],
        ),

        // DisableReviewsRequested while awaiting ancestry check -> cancel and disable
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha,
                check_run_id,
                ..
            },
            Event::DisableReviewsRequested,
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 1 },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled".to_string(),
                    summary: "Reviews were disabled for this PR.".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::ReviewsDisabled,
                    // Track batch for polling in case cancel fails
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // Enable reviews while awaiting ancestry check -> flip flag and acknowledge
        // The batch keeps running; user just enabled automatic reviews going forward.
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::EnableReviewsRequested { .. },
        ) => TransitionResult::new(
            ReviewMachineState::AwaitingAncestryCheck {
                reviews_enabled: true,
                batch_id: batch_id.clone(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id: *comment_id,
                check_run_id: *check_run_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                new_head_sha: new_head_sha.clone(),
                new_base_sha: new_base_sha.clone(),
                new_options: new_options.clone(),
            },
            vec![Effect::UpdateComment {
                content: CommentContent::ReviewsEnabled {
                    head_sha: head_sha.clone(),
                },
            }],
        ),

        // CancelRequested while awaiting ancestry check -> cancel batch
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::CancelRequested,
        ) => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::UserRequested,
                    },
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Review cancelled by user".to_string(),
                    summary: "The review was cancelled at the user's request.".to_string(),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    reason: CancellationReason::UserRequested,
                    // Track batch for polling in case cancel fails
                    pending_cancel_batch_id: Some(batch_id.clone()),
                },
                effects,
            )
        }

        // =====================================================================
        // Terminal State Transitions (Completed, Failed, Cancelled)
        // =====================================================================

        // New commit on terminal state -> start new review if enabled
        (
            ReviewMachineState::Completed {
                reviews_enabled: true,
                ..
            }
            | ReviewMachineState::Failed {
                reviews_enabled: true,
                ..
            }
            | ReviewMachineState::Cancelled {
                reviews_enabled: true,
                ..
            },
            Event::PrUpdated {
                head_sha,
                base_sha,
                options,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // New commit on terminal state with reviews disabled -> just log
        (
            ReviewMachineState::Completed {
                reviews_enabled: false,
                ..
            }
            | ReviewMachineState::Failed {
                reviews_enabled: false,
                ..
            }
            | ReviewMachineState::Cancelled {
                reviews_enabled: false,
                ..
            },
            Event::PrUpdated {
                head_sha,
                force_review: false,
                ..
            },
        ) => TransitionResult::new(
            state.clone(),
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewSuppressed {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::CreateCheckRun {
                    head_sha,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled".to_string(),
                    summary: "Reviews are disabled for this PR.".to_string(),
                },
            ],
        ),

        // Force review on terminal state -> start new review
        (
            terminal_state @ (ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled { .. }),
            Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: terminal_state.reviews_enabled(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // Enable reviews on terminal state -> mark enabled and start review
        (
            ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled { .. },
            Event::EnableReviewsRequested {
                head_sha,
                base_sha,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewsEnabled {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // Disable reviews on terminal state -> update flag
        (
            terminal @ (ReviewMachineState::Completed { head_sha, .. }
            | ReviewMachineState::Failed { head_sha, .. }
            | ReviewMachineState::Cancelled { head_sha, .. }),
            Event::DisableReviewsRequested,
        ) => {
            let new_state = match terminal {
                ReviewMachineState::Completed { result, .. } => ReviewMachineState::Completed {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    result: result.clone(),
                },
                ReviewMachineState::Failed { reason, .. } => ReviewMachineState::Failed {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    reason: reason.clone(),
                },
                ReviewMachineState::Cancelled {
                    reason,
                    pending_cancel_batch_id,
                    ..
                } => ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    reason: reason.clone(),
                    pending_cancel_batch_id: pending_cancel_batch_id.clone(),
                },
                _ => unreachable!(),
            };
            TransitionResult::new(
                new_state,
                vec![Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 0 },
                }],
            )
        }

        // Cancel on terminal state -> nothing to cancel
        (
            ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled { .. },
            Event::CancelRequested,
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::UpdateComment {
                content: CommentContent::NoReviewsToCancel,
            }],
        ),

        // Batch completed while in Cancelled state (cancel failed, batch completed anyway)
        // Clear the tracking - user already saw "cancelled" so we don't change the outcome
        (
            ReviewMachineState::Cancelled {
                reviews_enabled,
                head_sha,
                reason,
                pending_cancel_batch_id: Some(_),
            },
            Event::BatchCompleted { batch_id, .. } | Event::BatchTerminated { batch_id, .. },
        ) => TransitionResult::new(
            ReviewMachineState::Cancelled {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                reason: reason.clone(),
                // Clear the tracking - batch is done
                pending_cancel_batch_id: None,
            },
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Batch {} completed after cancel was requested, ignoring result",
                    batch_id
                ),
            }],
        ),

        // =====================================================================
        // Default: Unhandled event in this state
        // =====================================================================
        (_, event) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Warn,
                message: format!("Unhandled event {:?} in state {:?}", event, state),
            }],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{BatchId, CheckRunId, CommentId, CommitSha, ReviewOptions};
    use super::*;

    #[test]
    fn test_idle_to_preparing_on_pr_update() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                ..
            }
        ));
        assert_eq!(result.effects.len(), 1);
        assert!(matches!(result.effects[0], Effect::FetchData { .. }));
    }

    #[test]
    fn test_idle_disabled_suppresses_review() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = transition(state.clone(), event);

        // Should stay idle
        assert!(matches!(
            result.state,
            ReviewMachineState::Idle {
                reviews_enabled: false
            }
        ));
        // Should post suppression notice and create skipped check run
        assert_eq!(result.effects.len(), 2);
        assert!(matches!(
            &result.effects[0],
            Effect::UpdateComment {
                content: CommentContent::ReviewSuppressed { .. }
            }
        ));
        assert!(matches!(&result.effects[1], Effect::CreateCheckRun { .. }));
    }

    #[test]
    fn test_force_review_overrides_disabled() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should start preparing despite disabled
        assert!(matches!(
            result.state,
            ReviewMachineState::Preparing {
                reviews_enabled: false, // Preserves the disabled state
                ..
            }
        ));
    }

    #[test]
    fn test_batch_pending_to_completed() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_123".to_string()),
            result: ReviewResult::NoIssues {
                summary: "LGTM".to_string(),
                reasoning: "Code looks good".to_string(),
            },
        };

        let result = transition(state, event);

        assert!(matches!(result.state, ReviewMachineState::Completed { .. }));
        assert_eq!(result.effects.len(), 2);
        assert!(matches!(
            &result.effects[0],
            Effect::UpdateComment {
                content: CommentContent::ReviewComplete { .. }
            }
        ));
        assert!(matches!(&result.effects[1], Effect::UpdateCheckRun { .. }));
    }

    #[test]
    fn test_cancel_while_pending() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        let event = Event::CancelRequested;

        let result = transition(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::Cancelled {
                reason: CancellationReason::UserRequested,
                ..
            }
        ));
        assert_eq!(result.effects.len(), 3);
        assert!(matches!(&result.effects[0], Effect::CancelBatch { .. }));
    }

    #[test]
    fn test_new_commit_triggers_ancestry_check() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("new_commit"),
            base_sha: CommitSha::from("def456"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::AwaitingAncestryCheck { .. }
        ));
        assert_eq!(result.effects.len(), 1);
        assert!(matches!(&result.effects[0], Effect::CheckAncestry { .. }));
    }

    #[test]
    fn test_superseded_commit_cancels_and_starts_new() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("base_sha"),
            new_options: ReviewOptions::default(),
        };
        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: true,
        };

        let result = transition(state, event);

        // Should transition to Preparing for new commit
        assert!(matches!(
            result.state,
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                ..
            }
        ));
        if let ReviewMachineState::Preparing { head_sha, .. } = &result.state {
            assert_eq!(head_sha.0, "new_sha");
        }

        // Should cancel old batch and fetch new data
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::CancelBatch { .. })));
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::FetchData { .. })));
    }

    #[test]
    fn test_terminal_state_stable_for_same_commit() {
        let state = ReviewMachineState::Completed {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            result: ReviewResult::NoIssues {
                summary: "LGTM".to_string(),
                reasoning: "".to_string(),
            },
        };

        // Cancel should do nothing
        let result = transition(state.clone(), Event::CancelRequested);
        assert!(matches!(result.state, ReviewMachineState::Completed { .. }));
        assert!(matches!(
            &result.effects[0],
            Effect::UpdateComment {
                content: CommentContent::NoReviewsToCancel
            }
        ));
    }

    #[test]
    fn test_enable_reviews_triggers_new_review() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        let event = Event::EnableReviewsRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                ..
            }
        ));
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::UpdateComment { .. })));
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::FetchData { .. })));
    }

    #[test]
    fn test_reviews_enabled_preserved_through_transitions() {
        // Start with reviews disabled
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };

        // Force a review
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };
        let result = transition(state, event);

        // Should preserve reviews_enabled = false even though we're reviewing
        assert!(!result.state.reviews_enabled());
    }

    /// Regression test: When a commit is superseded, the new review should use
    /// the options from the PrUpdated event, not ReviewOptions::default().
    ///
    /// Bug: transition.rs:594 used ReviewOptions::default() instead of preserving
    /// the options from the new PR update, so custom model/reasoning settings were lost.
    #[test]
    fn test_superseded_commit_preserves_new_options() {
        // New commit pushed with custom options (model and reasoning_effort)
        let custom_options = ReviewOptions {
            model: Some("o3".to_string()),
            reasoning_effort: Some("high".to_string()),
        };

        // Start in BatchPending state
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };

        // New commit arrives with custom options
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("base_sha"),
            force_review: false,
            options: custom_options.clone(),
        };

        // Transition to AwaitingAncestryCheck
        let result = transition(state, event);
        assert!(matches!(
            result.state,
            ReviewMachineState::AwaitingAncestryCheck { .. }
        ));

        // Now simulate ancestry check confirming superseded
        let ancestry_event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: true,
        };
        let result = transition(result.state, ancestry_event);

        // Should transition to Preparing with the custom options preserved
        if let ReviewMachineState::Preparing { options, .. } = &result.state {
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Model option should be preserved from new PR update"
            );
            assert_eq!(
                options.reasoning_effort,
                Some("high".to_string()),
                "Reasoning effort option should be preserved from new PR update"
            );
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }
    }

    // =========================================================================
    // Regression Tests - These tests verify fixes for specific bugs
    // =========================================================================

    /// Regression test: When commits diverge (force-push/rebase), the new commit
    /// must not be dropped. The old batch should be cancelled and a new review
    /// started for the new commit.
    ///
    /// Bug: When `is_superseded: false` in AncestryResult, the code returned to
    /// BatchPending for the OLD head_sha, dropping the new commit entirely.
    #[test]
    fn test_diverged_commits_starts_new_review_not_dropped() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: None,
            },
        };

        // Commits diverged (not in ancestor relationship) - e.g., force-push
        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: false,
        };

        let result = transition(state, event);

        // Should transition to Preparing for the NEW commit, not stay on old
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(
                head_sha.0, "new_sha",
                "Should prepare review for new commit, not old"
            );
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use options from new commit"
            );
        } else {
            panic!(
                "Expected Preparing state for new commit, got {:?}",
                result.state
            );
        }

        // Should cancel the old batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the old batch"
        );

        // Should fetch data for the new commit
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { head_sha, .. } if head_sha.0 == "new_sha"
            )),
            "Should fetch data for new commit"
        );
    }

    /// Regression test: EnableReviewsRequested must honor model/reasoning options.
    ///
    /// Bug: The event didn't carry options and transitions hard-coded
    /// ReviewOptions::default(), losing custom model/reasoning settings.
    #[test]
    fn test_enable_reviews_honors_options() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };

        let custom_options = ReviewOptions {
            model: Some("o3".to_string()),
            reasoning_effort: Some("high".to_string()),
        };

        let event = Event::EnableReviewsRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: custom_options,
        };

        let result = transition(state, event);

        if let ReviewMachineState::Preparing { options, .. } = &result.state {
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Enable reviews should preserve model option"
            );
            assert_eq!(
                options.reasoning_effort,
                Some("high".to_string()),
                "Enable reviews should preserve reasoning_effort option"
            );
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }
    }

    /// Regression test: ReviewRequested must work in Preparing state.
    ///
    /// Bug: ReviewRequested was only handled in Idle/terminal states. In active
    /// states (Preparing/BatchPending/AwaitingAncestryCheck), it fell through to
    /// the default "unhandled event" handler and did nothing.
    #[test]
    fn test_review_requested_in_preparing_state() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            options: ReviewOptions::default(),
        };

        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("new_base"),
            options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: None,
            },
        };

        let result = transition(state, event);

        // Should restart with the new request
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(head_sha.0, "new_sha", "Should use new head SHA");
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use new options"
            );
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }

        // Should emit FetchData for the new commit
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { head_sha, .. } if head_sha.0 == "new_sha"
            )),
            "Should fetch data for new commit"
        );
    }

    /// Regression test: ReviewRequested must work in BatchPending state.
    #[test]
    fn test_review_requested_in_batch_pending_state() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };

        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("new_base"),
            options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        };

        let result = transition(state, event);

        // Should cancel current batch and start new review
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(head_sha.0, "new_sha", "Should use new head SHA");
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use new options"
            );
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }

        // Should cancel the old batch
        assert!(
            result.effects.iter().any(
                |e| matches!(e, Effect::CancelBatch { batch_id } if batch_id.0 == "batch_123")
            ),
            "Should cancel the pending batch"
        );
    }

    /// Regression test: ReviewRequested must work in AwaitingAncestryCheck state.
    #[test]
    fn test_review_requested_in_awaiting_ancestry_check_state() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("pending_sha"),
            new_base_sha: CommitSha::from("pending_base"),
            new_options: ReviewOptions::default(),
        };

        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("requested_sha"),
            base_sha: CommitSha::from("requested_base"),
            options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: None,
            },
        };

        let result = transition(state, event);

        // Should cancel current batch and start new review
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(head_sha.0, "requested_sha", "Should use requested head SHA");
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use requested options"
            );
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }

        // Should cancel the old batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the pending batch"
        );
    }

    /// Regression test: User cancel should set check-run conclusion to Skipped,
    /// not Cancelled.
    ///
    /// Bug: User cancel set conclusion to Cancelled, changing the semantics
    /// shown in GitHub from the previous behavior.
    #[test]
    fn test_user_cancel_sets_skipped_conclusion() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };

        let result = transition(state, Event::CancelRequested);

        // Find the UpdateCheckRun effect and verify conclusion is Skipped
        let check_run_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::UpdateCheckRun { .. }));

        assert!(
            check_run_effect.is_some(),
            "Should have UpdateCheckRun effect"
        );

        if let Some(Effect::UpdateCheckRun { conclusion, .. }) = check_run_effect {
            assert_eq!(
                *conclusion,
                Some(EffectCheckRunConclusion::Skipped),
                "User cancel should set conclusion to Skipped, not Cancelled"
            );
        }
    }

    /// Regression test: Externally cancelled batch should set check-run conclusion to Cancelled,
    /// not Failure or Skipped.
    ///
    /// Bug: When a batch is cancelled externally (via OpenAI dashboard/API), it was mapped to
    /// FailureReason::BatchFailed which resulted in a Failure conclusion. It should
    /// use BatchCancelled and Cancelled conclusion instead to accurately represent the outcome.
    #[test]
    fn test_cancelled_batch_sets_cancelled_conclusion() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };

        let result = transition(
            state,
            Event::BatchTerminated {
                batch_id: BatchId::from("batch_123".to_string()),
                reason: FailureReason::BatchCancelled,
            },
        );

        // Find the UpdateCheckRun effect and verify conclusion is Cancelled
        let check_run_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::UpdateCheckRun { .. }));

        assert!(
            check_run_effect.is_some(),
            "Should have UpdateCheckRun effect"
        );

        if let Some(Effect::UpdateCheckRun { conclusion, .. }) = check_run_effect {
            assert_eq!(
                *conclusion,
                Some(EffectCheckRunConclusion::Cancelled),
                "Externally cancelled batch should set conclusion to Cancelled"
            );
        }
    }

    // =========================================================================
    // Bug: Unhandled events in Preparing and AwaitingAncestryCheck states
    // These tests verify that events are not dropped in these states.
    // =========================================================================

    /// Regression test: PrUpdated should not be dropped while Preparing.
    ///
    /// Bug: When a new commit is pushed while in Preparing state (fetching data),
    /// the PrUpdated event was falling through to the default "unhandled" handler
    /// and being logged but not processed, causing the new commit to be ignored.
    #[test]
    fn test_pr_updated_while_preparing_not_dropped() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            options: ReviewOptions::default(),
        };

        let event = Event::PrUpdated {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("new_base"),
            force_review: false,
            options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: None,
            },
        };

        let result = transition(state, event);

        // Should restart with the new commit, NOT just log a warning
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )),
            "PrUpdated while Preparing should not just log a warning - it should be handled"
        );

        // Should transition to Preparing for the new commit
        if let ReviewMachineState::Preparing { head_sha, .. } = &result.state {
            assert_eq!(head_sha.0, "new_sha", "Should be preparing the new commit");
        } else {
            panic!(
                "Expected Preparing state for new commit, got {:?}",
                result.state
            );
        }

        // Should emit FetchData for the new commit
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { head_sha, .. } if head_sha.0 == "new_sha"
            )),
            "Should fetch data for new commit"
        );
    }

    /// Regression test: DisableReviewsRequested should not be dropped while Preparing.
    ///
    /// Bug: When reviews are disabled while in Preparing state, the event was
    /// falling through to the default "unhandled" handler and being logged
    /// but not processed, leaving the state unchanged.
    #[test]
    fn test_disable_reviews_while_preparing_not_dropped() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, Event::DisableReviewsRequested);

        // Should NOT just log a warning
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )),
            "DisableReviewsRequested while Preparing should not just log a warning"
        );

        // Should transition to Idle with reviews disabled
        assert!(
            matches!(
                result.state,
                ReviewMachineState::Idle {
                    reviews_enabled: false
                }
            ),
            "Should transition to Idle with reviews disabled"
        );

        // Should emit ReviewsDisabled comment
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { .. }
                }
            )),
            "Should emit ReviewsDisabled comment"
        );
    }

    // =========================================================================
    // Bug: Duplicate events while Preparing can cause double batch submission
    // Same-SHA events during Preparing should be ignored (de-duped).
    // =========================================================================

    /// Regression test: PrUpdated for the SAME commit while Preparing
    /// should NOT restart FetchData - it's a duplicate webhook.
    ///
    /// Bug: Without SHA comparison, concurrent webhooks for the same commit
    /// can each trigger FetchData, leading to multiple batch submissions.
    #[test]
    fn test_pr_updated_same_commit_while_preparing_does_not_restart() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        // PrUpdated for the SAME commit
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"), // Same as preparing
            base_sha: CommitSha::from("def456"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should NOT emit FetchData (would cause duplicate batch)
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::FetchData { .. })),
            "Should NOT restart FetchData for same commit - got: {:?}",
            result.effects
        );

        // Should stay in Preparing
        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Should stay in Preparing, got: {:?}",
            result.state
        );

        // Should log the duplicate (matching BatchPending pattern)
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Info,
                    ..
                }
            )),
            "Should log the duplicate event"
        );
    }

    /// Regression test: ReviewRequested for the SAME commit while Preparing
    /// should NOT restart FetchData - it's a duplicate request.
    ///
    /// Bug: Without SHA comparison, concurrent review requests for the same commit
    /// can each trigger FetchData, leading to multiple batch submissions.
    #[test]
    fn test_review_requested_same_commit_while_preparing_does_not_restart() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        // ReviewRequested for the SAME commit
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"), // Same as preparing
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should NOT emit FetchData (would cause duplicate batch)
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::FetchData { .. })),
            "Should NOT restart FetchData for same commit - got: {:?}",
            result.effects
        );

        // Should stay in Preparing
        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Should stay in Preparing, got: {:?}",
            result.state
        );

        // Should log the duplicate (matching BatchPending pattern)
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Info,
                    ..
                }
            )),
            "Should log the duplicate event"
        );
    }

    /// Regression test: PrUpdated should not be dropped while AwaitingAncestryCheck.
    ///
    /// Bug: When yet another new commit is pushed while waiting for ancestry
    /// check, the PrUpdated event was falling through to the default handler.
    #[test]
    fn test_pr_updated_while_awaiting_ancestry_not_dropped() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("pending_sha"),
            new_base_sha: CommitSha::from("pending_base"),
            new_options: ReviewOptions::default(),
        };

        let event = Event::PrUpdated {
            head_sha: CommitSha::from("even_newer_sha"),
            base_sha: CommitSha::from("even_newer_base"),
            force_review: false,
            options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: None,
            },
        };

        let result = transition(state, event);

        // Should NOT just log a warning
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )),
            "PrUpdated while AwaitingAncestryCheck should not just log a warning"
        );

        // Should update to track the newest commit (staying in AwaitingAncestryCheck
        // with updated new_head_sha, or immediately cancel and start fresh)
        // The important thing is the event is not dropped
        let tracks_newest = match &result.state {
            ReviewMachineState::AwaitingAncestryCheck { new_head_sha, .. } => {
                new_head_sha.0 == "even_newer_sha"
            }
            ReviewMachineState::Preparing { head_sha, .. } => head_sha.0 == "even_newer_sha",
            _ => false,
        };

        assert!(
            tracks_newest,
            "Should track the newest commit, got state: {:?}",
            result.state
        );
    }

    /// Regression test: DisableReviewsRequested should not be dropped while AwaitingAncestryCheck.
    ///
    /// Bug: When reviews are disabled while waiting for ancestry check, the
    /// batch should be cancelled and state should become Cancelled with reviews disabled.
    #[test]
    fn test_disable_reviews_while_awaiting_ancestry_not_dropped() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions::default(),
        };

        let result = transition(state, Event::DisableReviewsRequested);

        // Should NOT just log a warning
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )),
            "DisableReviewsRequested while AwaitingAncestryCheck should not just log a warning"
        );

        // Should have reviews_enabled = false
        assert!(
            !result.state.reviews_enabled(),
            "Should have reviews disabled"
        );

        // Should cancel the pending batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the pending batch"
        );
    }

    /// Regression test: CancelRequested should not be dropped while AwaitingAncestryCheck.
    ///
    /// Bug: When user requests cancel while waiting for ancestry check, the
    /// batch should be cancelled and state should become Cancelled.
    #[test]
    fn test_cancel_while_awaiting_ancestry_not_dropped() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions::default(),
        };

        let result = transition(state, Event::CancelRequested);

        // Should NOT just log a warning
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )),
            "CancelRequested while AwaitingAncestryCheck should not just log a warning"
        );

        // Should be in Cancelled state
        assert!(
            matches!(result.state, ReviewMachineState::Cancelled { .. }),
            "Should be in Cancelled state"
        );

        // Should cancel the pending batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the pending batch"
        );
    }

    // =========================================================================
    // Bug #1: CancelBatch failure drops batch from polling
    // These tests verify that batches are tracked even after cancel is requested,
    // so that if the cancel fails, we can still process the batch result.
    // =========================================================================

    /// Regression test: After CancelRequested, the batch should still be trackable
    /// for polling in case the cancel fails.
    ///
    /// Bug: When CancelRequested is processed, the state immediately transitions
    /// to Cancelled, which has has_pending_batch() = false. If the CancelBatch
    /// effect fails, the batch is lost from polling and its result is never processed.
    #[test]
    fn test_cancelled_state_tracks_batch_for_failed_cancel() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = transition(state, Event::CancelRequested);

        // State should be Cancelled
        assert!(matches!(result.state, ReviewMachineState::Cancelled { .. }));

        // But batch_id should still be trackable (for polling in case cancel fails)
        // This is the key assertion - currently this FAILS because Cancelled
        // doesn't track the batch_id
        assert!(
            result.state.pending_batch_id().is_some(),
            "Cancelled state should track batch_id in case cancel fails"
        );
        assert_eq!(
            result.state.pending_batch_id().map(|b| b.0.as_str()),
            Some("batch_123"),
            "Cancelled state should preserve the original batch_id"
        );
    }

    /// Regression test: DisableReviewsRequested while BatchPending should also
    /// track the batch for failed cancellation.
    #[test]
    fn test_disable_reviews_tracks_batch_for_failed_cancel() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_456".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = transition(state, Event::DisableReviewsRequested);

        assert!(matches!(result.state, ReviewMachineState::Cancelled { .. }));
        assert!(
            result.state.pending_batch_id().is_some(),
            "Cancelled state should track batch_id after disable-reviews"
        );
    }

    // =========================================================================
    // Bug #3: Polled cancelled batch shows as Failed/ReviewFailed
    // A batch that is externally cancelled should show as Cancelled, not Failed.
    // =========================================================================

    /// Regression test: When polling finds a cancelled batch (external cancellation),
    /// it should transition to Cancelled state, not Failed.
    ///
    /// Bug: BatchTerminated with BatchCancelled reason transitions to Failed state
    /// and posts ReviewFailed comment. Should transition to Cancelled with
    /// ReviewCancelled comment.
    #[test]
    fn test_externally_cancelled_batch_becomes_cancelled_not_failed() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let event = Event::BatchTerminated {
            batch_id: BatchId::from("batch_123".to_string()),
            reason: FailureReason::BatchCancelled,
        };

        let result = transition(state, event);

        // Should be Cancelled, not Failed
        assert!(
            matches!(result.state, ReviewMachineState::Cancelled { .. }),
            "Externally cancelled batch should transition to Cancelled state, got: {:?}",
            result.state
        );

        // Should post ReviewCancelled, not ReviewFailed
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled { .. }
                }
            )),
            "Should post ReviewCancelled comment for externally cancelled batch"
        );
    }

    /// Regression test: When a batch completes but we're in Cancelled state
    /// (because cancel failed), we should still handle it gracefully.
    #[test]
    fn test_cancelled_state_handles_batch_completed() {
        // Cancelled state with pending batch (cancel was requested but may have failed)
        let state = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            reason: CancellationReason::UserRequested,
            pending_cancel_batch_id: Some(BatchId::from("batch_123".to_string())),
        };

        // If a BatchCompleted event arrives (because cancel_batch failed
        // and the batch completed anyway), we should handle it gracefully
        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_123".to_string()),
            result: ReviewResult::NoIssues {
                summary: "LGTM".to_string(),
                reasoning: "All good".to_string(),
            },
        };

        let result = transition(state, event);

        // Should NOT fall through to default handler (which logs a warning)
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    message
                } if message.contains("Unhandled")
            )),
            "BatchCompleted in Cancelled state should be handled, not logged as unhandled"
        );

        // Should clear the pending_cancel_batch_id since the batch is now done
        match &result.state {
            ReviewMachineState::Cancelled {
                pending_cancel_batch_id,
                ..
            } => {
                assert_eq!(
                    *pending_cancel_batch_id, None,
                    "pending_cancel_batch_id should be cleared after batch completes"
                );
            }
            other => panic!("Expected Cancelled state, got {:?}", other),
        }

        // Should log an Info message about the batch completing after cancel
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Info,
                    ..
                }
            )),
            "Should log info about batch completing after cancel was requested"
        );
    }

    // =========================================================================
    // Bug: BatchCompleted/BatchTerminated not handled in AwaitingAncestryCheck
    // When a batch completes while we're checking ancestry (to see if a new
    // commit supersedes the old one), the event falls through to the default
    // handler and results are lost.
    // =========================================================================

    /// Regression test: BatchCompleted while AwaitingAncestryCheck must be handled.
    ///
    /// Bug: The batch can complete while we're waiting for the ancestry check.
    /// Since AwaitingAncestryCheck has a pending_batch_id(), the polling loop
    /// will generate BatchCompleted events, but there's no handler for them.
    /// The event falls through to the default handler and results are discarded.
    #[test]
    fn test_batch_completed_while_awaiting_ancestry_check() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base_sha"),
            new_options: ReviewOptions::default(),
        };

        // Batch completes while we're waiting for ancestry check
        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_123".to_string()),
            result: ReviewResult::NoIssues {
                summary: "LGTM".to_string(),
                reasoning: "Code looks good".to_string(),
            },
        };

        let result = transition(state, event);

        // Should NOT fall through to default handler (which just logs a warning)
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    message
                } if message.contains("Unhandled")
            )),
            "BatchCompleted in AwaitingAncestryCheck should be handled, not logged as unhandled"
        );

        // Should transition to a state that reflects the batch completed
        // (either Completed if we process the result, or some intermediate state)
        assert!(
            !matches!(result.state, ReviewMachineState::AwaitingAncestryCheck { .. }),
            "Should not stay in AwaitingAncestryCheck after batch completes - results would be lost"
        );
    }

    /// Regression test: BatchTerminated while AwaitingAncestryCheck must be handled.
    #[test]
    fn test_batch_terminated_while_awaiting_ancestry_check() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base_sha"),
            new_options: ReviewOptions::default(),
        };

        // Batch fails/expires while we're waiting for ancestry check
        let event = Event::BatchTerminated {
            batch_id: BatchId::from("batch_123".to_string()),
            reason: FailureReason::BatchExpired,
        };

        let result = transition(state, event);

        // Should NOT fall through to default handler
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log {
                    level: LogLevel::Warn,
                    message
                } if message.contains("Unhandled")
            )),
            "BatchTerminated in AwaitingAncestryCheck should be handled, not logged as unhandled"
        );
    }

    // =========================================================================
    // Bug #4: Compare-commits errors - prioritize reviewing the latest commit
    // When ancestry check fails (GitHub API error), we can't determine if the
    // new commit supersedes the old one. Rather than risk missing the new commit,
    // we cancel the old batch and start a new review for the latest commit.
    // =========================================================================

    /// Test that ancestry check failure cancels old batch and starts new review.
    ///
    /// When ancestry check fails (GitHub API error), we prioritize reviewing
    /// the latest commit. The old batch is cancelled and a new review starts.
    #[test]
    fn test_ancestry_check_failure_cancels_old_and_reviews_new() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base_sha"),
            new_options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: Some("xhigh".to_string()),
            },
        };

        let event = Event::AncestryCheckFailed {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            error: "GitHub API rate limited".to_string(),
        };

        let result = transition(state, event);

        // Should transition to Preparing for the new commit
        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Ancestry check failure should start preparing new review, got: {:?}",
            result.state
        );

        // Should cancel the old batch
        assert!(
            result.effects.iter().any(
                |e| matches!(e, Effect::CancelBatch { batch_id } if batch_id.0 == "batch_123")
            ),
            "Ancestry check failure should cancel the old batch"
        );

        // New commit should be prepared for review
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(head_sha.0, "new_sha", "Should prepare new commit");
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use new options"
            );
        }
    }

    // =========================================================================
    // Bug #5: reviews_enabled not enforced for PrUpdated while BatchPending
    // When reviews are disabled, new commits should not start new reviews.
    // =========================================================================

    /// Regression test: When reviews are disabled and commits diverge (force-push),
    /// the old batch should be cancelled but NO new review should start.
    ///
    /// Bug: AncestryResult with is_superseded=false always starts a new review,
    /// even when reviews_enabled=false.
    #[test]
    fn test_disabled_reviews_no_new_review_on_force_push() {
        // Batch is pending with reviews_enabled=false (from a forced review)
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: false, // Reviews are disabled!
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base_sha"),
            new_options: ReviewOptions::default(),
        };

        // Force-push detected: old commit is NOT superseded
        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: false,
        };

        let result = transition(state, event);

        // Should NOT go to Preparing (which would start a new review)
        assert!(
            !matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Should NOT start new review when reviews are disabled, got: {:?}",
            result.state
        );

        // Should cancel the old batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the old batch"
        );

        // Should NOT start fetching data for new review
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::FetchData { .. })),
            "Should NOT fetch data for new review when reviews are disabled"
        );
    }

    // =========================================================================
    // Bug: Title exceeds GitHub limits (max 255 chars)
    // When BatchCompleted produces a summary, the title must be truncated.
    // =========================================================================

    /// Regression test: Check run title must not exceed GitHub's 255 char limit.
    ///
    /// Bug: The title was built with `format!("Code review found issues: {}", summary)`
    /// which can produce arbitrarily long titles when the model summary is long or multiline.
    /// GitHub rejects updates with titles > 255 chars, leaving check runs stuck in_progress.
    #[test]
    fn test_batch_completed_title_truncated_for_long_summary() {
        let long_summary = "A".repeat(300); // Way over 255 chars

        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_123".to_string()),
            result: ReviewResult::HasIssues {
                summary: long_summary,
                reasoning: "Detailed reasoning".to_string(),
                comments: vec![],
            },
        };

        let result = transition(state, event);

        // Find the UpdateCheckRun effect
        let check_run_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::UpdateCheckRun { .. }));

        assert!(
            check_run_effect.is_some(),
            "Should have UpdateCheckRun effect"
        );

        if let Some(Effect::UpdateCheckRun { title, .. }) = check_run_effect {
            assert!(
                title.len() <= 255,
                "Title must not exceed GitHub's 255 char limit, but got {} chars: '{}'",
                title.len(),
                title
            );
        }
    }

    /// Test that multiline summaries are handled correctly in titles.
    #[test]
    fn test_batch_completed_title_handles_multiline_summary() {
        let multiline_summary = "Line 1: Some issue\nLine 2: Another issue\nLine 3: Yet another";

        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_123".to_string()),
            result: ReviewResult::HasIssues {
                summary: multiline_summary.to_string(),
                reasoning: "Reasoning".to_string(),
                comments: vec![],
            },
        };

        let result = transition(state, event);

        if let Some(Effect::UpdateCheckRun { title, .. }) = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::UpdateCheckRun { .. }))
        {
            // Title should not contain newlines
            assert!(
                !title.contains('\n'),
                "Title should not contain newlines, got: '{}'",
                title
            );
        }
    }

    // =========================================================================
    // Bug: @robocop review while batch pending for same commit restarts
    // De-dup: If the requested commit matches the pending batch, no-op.
    // =========================================================================

    /// Regression test: ReviewRequested for the SAME commit as pending batch
    /// should not cancel and restart - it's a duplicate request.
    ///
    /// Bug: ReviewRequested while BatchPending always cancels and restarts,
    /// even if the requested commit is the same as the one being reviewed.
    /// This doubles OpenAI spend for repeated manual requests.
    #[test]
    fn test_review_requested_same_commit_does_not_restart() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        // Request review for the SAME commit
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"), // Same as pending
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should NOT cancel the batch
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should NOT cancel batch when ReviewRequested is for same commit"
        );

        // Should stay in BatchPending
        assert!(
            matches!(result.state, ReviewMachineState::BatchPending { .. }),
            "Should stay in BatchPending, got: {:?}",
            result.state
        );
    }

    // =========================================================================
    // Bug: EnableReviewsRequested not handled in BatchPending/AwaitingAncestryCheck
    // User enabling reviews during a forced review should acknowledge and flip flag.
    // =========================================================================

    /// Regression test: EnableReviewsRequested in BatchPending state should be handled,
    /// not fall through to "Unhandled event".
    ///
    /// Bug: When a forced review is running (reviews_enabled=false) and user enables
    /// reviews, the EnableReviewsRequested event falls through to default handler.
    #[test]
    fn test_enable_reviews_in_batch_pending_handled() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: false, // Forced review
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let event = Event::EnableReviewsRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should NOT just log a warning (unhandled event)
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log { level: LogLevel::Warn, message } if message.contains("Unhandled")
            )),
            "EnableReviewsRequested in BatchPending should not fall through to unhandled"
        );

        // Should set reviews_enabled = true
        assert!(
            result.state.reviews_enabled(),
            "Should have reviews enabled after EnableReviewsRequested"
        );
    }

    /// Regression test: EnableReviewsRequested in AwaitingAncestryCheck state should be handled.
    #[test]
    fn test_enable_reviews_in_awaiting_ancestry_handled() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: false, // Forced review
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions::default(),
        };

        let event = Event::EnableReviewsRequested {
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("base_sha"),
            options: ReviewOptions::default(),
        };

        let result = transition(state, event);

        // Should NOT just log a warning (unhandled event)
        assert!(
            !result.effects.iter().any(|e| matches!(
                e,
                Effect::Log { level: LogLevel::Warn, message } if message.contains("Unhandled")
            )),
            "EnableReviewsRequested in AwaitingAncestryCheck should not fall through to unhandled"
        );

        // Should set reviews_enabled = true
        assert!(
            result.state.reviews_enabled(),
            "Should have reviews enabled after EnableReviewsRequested"
        );
    }

    // =========================================================================
    // Regression Tests - Bugs found in code review
    // =========================================================================

    /// Regression test: AncestryCheckFailed must not drop the new commit.
    ///
    /// Bug: When ancestry check fails (GitHub API error), the transition returned
    /// to BatchPending for the OLD head_sha and dropped the new commit entirely.
    /// This means the new commit would never be reviewed unless another PrUpdated
    /// event arrived.
    ///
    /// The fix: When ancestry check fails, we should cancel the old batch and
    /// start a review for the new commit. This ensures the latest commit is always
    /// reviewed, even if we couldn't determine the ancestry relationship.
    #[test]
    fn test_ancestry_check_failed_reviews_new_commit() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions {
                model: Some("o3".to_string()),
                reasoning_effort: Some("high".to_string()),
            },
        };

        let event = Event::AncestryCheckFailed {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            error: "GitHub API error".to_string(),
        };

        let result = transition(state, event);

        // The new commit should be reviewed, not dropped
        // We should transition to Preparing for the NEW commit
        if let ReviewMachineState::Preparing {
            head_sha, options, ..
        } = &result.state
        {
            assert_eq!(
                head_sha.0, "new_sha",
                "Should prepare review for new commit, not stay on old"
            );
            assert_eq!(
                options.model,
                Some("o3".to_string()),
                "Should use options from new commit"
            );
        } else {
            panic!(
                "Expected Preparing state for new commit, got {:?}. \
                 Bug: AncestryCheckFailed is dropping the new commit!",
                result.state
            );
        }

        // Should cancel the old batch (since we're starting a new review)
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the old batch"
        );

        // Should fetch data for the new commit
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { head_sha, .. } if head_sha.0 == "new_sha"
            )),
            "Should fetch data for new commit"
        );
    }

    /// Regression test: When reviews are disabled and commit is superseded,
    /// the PR comment must be updated.
    ///
    /// Bug: When in AwaitingAncestryCheck with reviews_enabled=false and
    /// AncestryResult indicates superseded, the batch was cancelled and
    /// check run updated, but the PR comment was NOT updated. This leaves
    /// the visible comment in "in progress" state forever.
    #[test]
    fn test_superseded_with_reviews_disabled_updates_comment() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: false,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions::default(),
        };

        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: true,
        };

        let result = transition(state, event);

        // Should update the PR comment to indicate cancellation
        let has_comment_update = result.effects.iter().any(|e| {
            matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled { .. }
                }
            )
        });

        assert!(
            has_comment_update,
            "Should update PR comment when batch is cancelled due to supersession. \
             Bug: Comment remains 'in progress' forever when reviews disabled + superseded!"
        );
    }

    /// Regression test: When reviews are disabled and commits diverge (not superseded),
    /// the PR comment must be updated.
    ///
    /// Same bug as above but for the diverged case (is_superseded=false).
    #[test]
    fn test_diverged_with_reviews_disabled_updates_comment() {
        let state = ReviewMachineState::AwaitingAncestryCheck {
            reviews_enabled: false,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
            new_head_sha: CommitSha::from("new_sha"),
            new_base_sha: CommitSha::from("new_base"),
            new_options: ReviewOptions::default(),
        };

        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: false,
        };

        let result = transition(state, event);

        // Should update the PR comment to indicate cancellation
        let has_comment_update = result.effects.iter().any(|e| {
            matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled { .. }
                }
            )
        });

        assert!(
            has_comment_update,
            "Should update PR comment when batch is cancelled due to divergence. \
             Bug: Comment remains 'in progress' forever when reviews disabled + diverged!"
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::super::state::{BatchId, CheckRunId, CommentId, CommitSha, ReviewOptions};
    use super::*;
    use proptest::prelude::*;

    // =========================================================================
    // Arbitrary generators
    // =========================================================================

    fn arb_commit_sha() -> impl Strategy<Value = CommitSha> {
        "[a-f0-9]{40}".prop_map(CommitSha::from)
    }

    fn arb_batch_id() -> impl Strategy<Value = BatchId> {
        "batch_[a-zA-Z0-9]{24}".prop_map(BatchId::from)
    }

    fn arb_comment_id() -> impl Strategy<Value = CommentId> {
        (1u64..1000000).prop_map(CommentId::from)
    }

    fn arb_check_run_id() -> impl Strategy<Value = Option<CheckRunId>> {
        proptest::option::of((1u64..1000000).prop_map(CheckRunId::from))
    }

    fn arb_model() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("o1".to_string()),
            Just("o3".to_string()),
            Just("gpt-4".to_string()),
            Just("gpt-4o".to_string()),
        ]
    }

    fn arb_reasoning_effort() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("low".to_string()),
            Just("medium".to_string()),
            Just("high".to_string()),
        ]
    }

    fn arb_review_options() -> impl Strategy<Value = ReviewOptions> {
        (
            proptest::option::of(arb_model()),
            proptest::option::of(arb_reasoning_effort()),
        )
            .prop_map(|(model, reasoning_effort)| ReviewOptions {
                model,
                reasoning_effort,
            })
    }

    fn arb_review_result() -> impl Strategy<Value = ReviewResult> {
        prop_oneof![
            (".*", ".*")
                .prop_map(|(summary, reasoning)| ReviewResult::NoIssues { summary, reasoning }),
            (".*", ".*").prop_map(|(summary, reasoning)| ReviewResult::HasIssues {
                summary,
                reasoning,
                comments: vec![], // Simplified for now
            }),
        ]
    }

    fn arb_failure_reason() -> impl Strategy<Value = FailureReason> {
        prop_oneof![
            Just(FailureReason::BatchExpired),
            Just(FailureReason::BatchCancelled),
            Just(FailureReason::NoOutputFile),
            ".*".prop_map(|e| FailureReason::BatchFailed { error: Some(e) }),
            ".*".prop_map(|e| FailureReason::DownloadFailed { error: e }),
            ".*".prop_map(|e| FailureReason::ParseFailed { error: e }),
            ".*".prop_map(|e| FailureReason::SubmissionFailed { error: e }),
        ]
    }

    fn arb_cancellation_reason() -> impl Strategy<Value = CancellationReason> {
        prop_oneof![
            Just(CancellationReason::UserRequested),
            Just(CancellationReason::ReviewsDisabled),
            arb_commit_sha().prop_map(|sha| CancellationReason::Superseded { new_sha: sha }),
        ]
    }

    fn arb_idle_state() -> impl Strategy<Value = ReviewMachineState> {
        any::<bool>().prop_map(|reviews_enabled| ReviewMachineState::Idle { reviews_enabled })
    }

    fn arb_preparing_state() -> impl Strategy<Value = ReviewMachineState> {
        (
            any::<bool>(),
            arb_commit_sha(),
            arb_commit_sha(),
            arb_review_options(),
        )
            .prop_map(|(reviews_enabled, head_sha, base_sha, options)| {
                ReviewMachineState::Preparing {
                    reviews_enabled,
                    head_sha,
                    base_sha,
                    options,
                }
            })
    }

    fn arb_batch_pending_state() -> impl Strategy<Value = ReviewMachineState> {
        (
            any::<bool>(),
            arb_batch_id(),
            arb_commit_sha(),
            arb_commit_sha(),
            proptest::option::of(arb_comment_id()),
            arb_check_run_id(),
            arb_model(),
            arb_reasoning_effort(),
        )
            .prop_map(
                |(
                    reviews_enabled,
                    batch_id,
                    head_sha,
                    base_sha,
                    comment_id,
                    check_run_id,
                    model,
                    reasoning_effort,
                )| {
                    ReviewMachineState::BatchPending {
                        reviews_enabled,
                        batch_id,
                        head_sha,
                        base_sha,
                        comment_id,
                        check_run_id,
                        model,
                        reasoning_effort,
                    }
                },
            )
    }

    fn arb_awaiting_ancestry_check_state() -> impl Strategy<Value = ReviewMachineState> {
        (
            any::<bool>(),
            arb_batch_id(),
            arb_commit_sha(),
            arb_commit_sha(),
            proptest::option::of(arb_comment_id()),
            arb_check_run_id(),
            arb_model(),
            arb_reasoning_effort(),
            arb_commit_sha(),
            arb_commit_sha(),
            arb_review_options(),
        )
            .prop_map(
                |(
                    reviews_enabled,
                    batch_id,
                    head_sha,
                    base_sha,
                    comment_id,
                    check_run_id,
                    model,
                    reasoning_effort,
                    new_head_sha,
                    new_base_sha,
                    new_options,
                )| {
                    ReviewMachineState::AwaitingAncestryCheck {
                        reviews_enabled,
                        batch_id,
                        head_sha,
                        base_sha,
                        comment_id,
                        check_run_id,
                        model,
                        reasoning_effort,
                        new_head_sha,
                        new_base_sha,
                        new_options,
                    }
                },
            )
    }

    fn arb_completed_state() -> impl Strategy<Value = ReviewMachineState> {
        (any::<bool>(), arb_commit_sha(), arb_review_result()).prop_map(
            |(reviews_enabled, head_sha, result)| ReviewMachineState::Completed {
                reviews_enabled,
                head_sha,
                result,
            },
        )
    }

    fn arb_failed_state() -> impl Strategy<Value = ReviewMachineState> {
        (any::<bool>(), arb_commit_sha(), arb_failure_reason()).prop_map(
            |(reviews_enabled, head_sha, reason)| ReviewMachineState::Failed {
                reviews_enabled,
                head_sha,
                reason,
            },
        )
    }

    fn arb_cancelled_state() -> impl Strategy<Value = ReviewMachineState> {
        (
            any::<bool>(),
            arb_commit_sha(),
            arb_cancellation_reason(),
            proptest::option::of(arb_batch_id()),
        )
            .prop_map(
                |(reviews_enabled, head_sha, reason, pending_cancel_batch_id)| {
                    ReviewMachineState::Cancelled {
                        reviews_enabled,
                        head_sha,
                        reason,
                        pending_cancel_batch_id,
                    }
                },
            )
    }

    fn arb_terminal_state() -> impl Strategy<Value = ReviewMachineState> {
        prop_oneof![
            arb_completed_state(),
            arb_failed_state(),
            arb_cancelled_state(),
        ]
    }

    fn arb_state() -> impl Strategy<Value = ReviewMachineState> {
        prop_oneof![
            arb_idle_state(),
            arb_preparing_state(),
            arb_batch_pending_state(),
            arb_awaiting_ancestry_check_state(),
            arb_completed_state(),
            arb_failed_state(),
            arb_cancelled_state(),
        ]
    }

    fn arb_pr_updated_event() -> impl Strategy<Value = Event> {
        (
            arb_commit_sha(),
            arb_commit_sha(),
            any::<bool>(),
            arb_review_options(),
        )
            .prop_map(
                |(head_sha, base_sha, force_review, options)| Event::PrUpdated {
                    head_sha,
                    base_sha,
                    force_review,
                    options,
                },
            )
    }

    fn arb_event() -> impl Strategy<Value = Event> {
        prop_oneof![
            arb_pr_updated_event(),
            (arb_commit_sha(), arb_commit_sha(), arb_review_options()).prop_map(
                |(head_sha, base_sha, options)| Event::ReviewRequested {
                    head_sha,
                    base_sha,
                    options
                }
            ),
            Just(Event::CancelRequested),
            (arb_commit_sha(), arb_commit_sha(), arb_review_options()).prop_map(
                |(head_sha, base_sha, options)| Event::EnableReviewsRequested {
                    head_sha,
                    base_sha,
                    options,
                },
            ),
            Just(Event::DisableReviewsRequested),
            (arb_batch_id(), arb_review_result())
                .prop_map(|(batch_id, result)| Event::BatchCompleted { batch_id, result }),
            (arb_batch_id(), arb_failure_reason())
                .prop_map(|(batch_id, reason)| Event::BatchTerminated { batch_id, reason }),
        ]
    }

    // =========================================================================
    // Property Tests
    // =========================================================================

    proptest! {
        /// Property: reviews_enabled only changes on explicit enable/disable commands
        #[test]
        fn reviews_enabled_only_changes_on_explicit_commands(
            state in arb_state(),
            event in arb_event()
        ) {
            let original_enabled = state.reviews_enabled();
            let result = transition(state, event.clone());
            let new_enabled = result.state.reviews_enabled();

            // reviews_enabled should only change if the event was Enable/Disable
            let is_enable_disable = matches!(
                event,
                Event::EnableReviewsRequested { .. } | Event::DisableReviewsRequested
            );

            if !is_enable_disable {
                prop_assert_eq!(
                    original_enabled, new_enabled,
                    "reviews_enabled changed without enable/disable command"
                );
            }
        }

        /// Property: CancelBatch effect only emitted when in BatchPending or AwaitingAncestryCheck
        #[test]
        fn cancel_batch_only_when_pending(
            state in arb_state(),
            event in arb_event()
        ) {
            let has_pending_batch = state.has_pending_batch();
            let result = transition(state, event);

            let emits_cancel = result.effects.iter().any(|e| matches!(e, Effect::CancelBatch { .. }));

            if emits_cancel {
                prop_assert!(
                    has_pending_batch,
                    "CancelBatch emitted but state had no pending batch"
                );
            }
        }

        /// Property: Terminal states remain terminal on cancel requests
        #[test]
        fn terminal_states_stable_on_cancel(
            state in arb_terminal_state()
        ) {
            let result = transition(state.clone(), Event::CancelRequested);

            // Should stay in same terminal state type
            prop_assert!(result.state.is_terminal(), "Terminal state became non-terminal on cancel");

            // Should have no pending batch effects
            let has_cancel_batch = result.effects.iter().any(|e| matches!(e, Effect::CancelBatch { .. }));
            prop_assert!(!has_cancel_batch, "Cancel emitted for terminal state");
        }

        /// Property: Transition function always produces valid state
        #[test]
        fn transition_produces_valid_state(
            state in arb_state(),
            event in arb_event()
        ) {
            let result = transition(state, event);

            // State should be constructible (type system ensures this, but let's verify no panics)
            let _ = result.state.reviews_enabled();
            let _ = result.state.is_terminal();
            let _ = result.state.has_pending_batch();

            // Effects should be non-panicking to inspect
            for effect in &result.effects {
                let _ = format!("{:?}", effect);
            }
        }

        /// Property: When a new commit arrives while BatchPending, either:
        /// 1. State stays BatchPending (same commit - duplicate webhook)
        /// 2. State becomes AwaitingAncestryCheck (different commit)
        #[test]
        fn new_commit_during_batch_pending_triggers_ancestry_check(
            state in arb_batch_pending_state(),
            new_head_sha in arb_commit_sha(),
            new_base_sha in arb_commit_sha(),
        ) {
            let current_head = if let ReviewMachineState::BatchPending { head_sha, .. } = &state {
                head_sha.clone()
            } else {
                unreachable!()
            };

            let event = Event::PrUpdated {
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha,
                force_review: false,
                options: ReviewOptions::default(),
            };

            let result = transition(state, event);

            if current_head == new_head_sha {
                // Same commit - should stay BatchPending
                prop_assert!(
                    matches!(result.state, ReviewMachineState::BatchPending { .. }),
                    "Same commit should stay BatchPending"
                );
            } else {
                // Different commit - should go to AwaitingAncestryCheck
                prop_assert!(
                    matches!(result.state, ReviewMachineState::AwaitingAncestryCheck { .. }),
                    "Different commit should trigger ancestry check"
                );
                // Should emit CheckAncestry effect
                prop_assert!(
                    result.effects.iter().any(|e| matches!(e, Effect::CheckAncestry { .. })),
                    "Should emit CheckAncestry effect"
                );
            }
        }

        /// Property: ReviewRequested always starts a review, even when disabled
        #[test]
        fn review_requested_always_starts_review(
            reviews_enabled in any::<bool>(),
            head_sha in arb_commit_sha(),
            base_sha in arb_commit_sha(),
            options in arb_review_options(),
        ) {
            let state = ReviewMachineState::Idle { reviews_enabled };
            let event = Event::ReviewRequested {
                head_sha,
                base_sha,
                options,
            };

            let result = transition(state, event);

            prop_assert!(
                matches!(result.state, ReviewMachineState::Preparing { .. }),
                "ReviewRequested should always start preparing"
            );
            prop_assert!(
                result.effects.iter().any(|e| matches!(e, Effect::FetchData { .. })),
                "Should emit FetchData effect"
            );
        }

        /// Property: Idle with reviews_enabled=false suppresses PrUpdated (unless force_review)
        #[test]
        fn disabled_idle_suppresses_pr_updated(
            head_sha in arb_commit_sha(),
            base_sha in arb_commit_sha(),
            options in arb_review_options(),
        ) {
            let state = ReviewMachineState::Idle { reviews_enabled: false };
            let event = Event::PrUpdated {
                head_sha,
                base_sha,
                force_review: false,
                options,
            };

            let result = transition(state, event);

            // Should stay Idle
            prop_assert!(
                matches!(result.state, ReviewMachineState::Idle { reviews_enabled: false }),
                "Should stay Idle when disabled"
            );
            // Should emit suppression notice
            prop_assert!(
                result.effects.iter().any(|e| matches!(e, Effect::UpdateComment { content: CommentContent::ReviewSuppressed { .. } })),
                "Should emit suppression notice"
            );
        }
    }
}
