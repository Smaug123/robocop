//! Pure state transition function.
//!
//! The transition function is the core of the state machine. It takes the
//! current state and an event, and returns the new state and a list of effects.
//! This function has NO side effects - it is pure and deterministic.

use super::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use super::event::{DataFetchFailure, Event};
use super::state::{
    CancellationReason, FailureReason, ReviewMachineState, ReviewOptions, ReviewResult,
};

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
        (ReviewMachineState::Idle { .. }, Event::EnableReviewsRequested { head_sha, base_sha }) => {
            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: true,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    options: ReviewOptions::default(),
                },
                vec![
                    Effect::UpdateComment {
                        content: CommentContent::ReviewsEnabled {
                            head_sha: head_sha.clone(),
                        },
                    },
                    Effect::FetchData { head_sha, base_sha },
                ],
            )
        }

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
        ) => TransitionResult::new(
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
            // Update comment to include the batch ID (initially created without it)
            vec![Effect::UpdateComment {
                content: CommentContent::InProgress {
                    head_sha: head_sha.clone(),
                    batch_id,
                    model,
                    reasoning_effort,
                },
            }],
        ),

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
            let title = match &result {
                ReviewResult::NoIssues { .. } => "Code review passed".to_string(),
                ReviewResult::HasIssues { summary, .. } => {
                    format!("Code review found issues: {}", summary)
                }
            };

            TransitionResult::new(
                ReviewMachineState::Completed {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    result: result.clone(),
                },
                vec![
                    Effect::UpdateComment {
                        content: CommentContent::ReviewComplete {
                            head_sha: head_sha.clone(),
                            batch_id: batch_id.clone(),
                            result,
                        },
                    },
                    Effect::UpdateCheckRun {
                        check_run_id: *check_run_id,
                        status: EffectCheckRunStatus::Completed,
                        conclusion: Some(conclusion),
                        title,
                        summary: "Review complete".to_string(),
                    },
                ],
            )
        }

        // Batch terminated (failed/expired/cancelled) -> transition to Failed
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

            TransitionResult::new(
                ReviewMachineState::Failed {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    reason: reason.clone(),
                },
                vec![
                    Effect::UpdateComment {
                        content: CommentContent::ReviewFailed {
                            head_sha: head_sha.clone(),
                            batch_id: batch_id.clone(),
                            reason,
                        },
                    },
                    Effect::UpdateCheckRun {
                        check_run_id: *check_run_id,
                        status: EffectCheckRunStatus::Completed,
                        conclusion: Some(conclusion),
                        title: "Code review failed".to_string(),
                        summary: "Review failed to complete".to_string(),
                    },
                ],
            )
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
        ) => TransitionResult::new(
            ReviewMachineState::Cancelled {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                reason: CancellationReason::UserRequested,
            },
            vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::UserRequested,
                    },
                },
                Effect::UpdateCheckRun {
                    check_run_id: *check_run_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Cancelled),
                    title: "Review cancelled by user".to_string(),
                    summary: "The review was cancelled at the user's request.".to_string(),
                },
            ],
        ),

        // Disable reviews while batch pending -> cancel and disable
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                check_run_id,
                ..
            },
            Event::DisableReviewsRequested,
        ) => TransitionResult::new(
            ReviewMachineState::Cancelled {
                reviews_enabled: false,
                head_sha: head_sha.clone(),
                reason: CancellationReason::ReviewsDisabled,
            },
            vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 1 },
                },
                Effect::UpdateCheckRun {
                    check_run_id: *check_run_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Skipped),
                    title: "Reviews disabled".to_string(),
                    summary: "Reviews were disabled for this PR.".to_string(),
                },
            ],
        ),

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

        // Old commit is superseded -> cancel old, start new review
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
                is_superseded: true,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: *reviews_enabled,
                head_sha: new_head_sha.clone(),
                base_sha: new_base_sha.clone(),
                options: new_options.clone(),
            },
            vec![
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
                Effect::UpdateCheckRun {
                    check_run_id: *check_run_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "This review was superseded by a newer commit: {}",
                        new_head_sha.short()
                    ),
                },
                Effect::FetchData {
                    head_sha: new_head_sha.clone(),
                    base_sha: new_base_sha.clone(),
                },
            ],
        ),

        // Old commit is NOT superseded (parallel development) -> stay pending
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                reviews_enabled,
                model,
                reasoning_effort,
                ..
            },
            Event::AncestryResult {
                is_superseded: false,
                ..
            },
        ) => TransitionResult::new(
            ReviewMachineState::BatchPending {
                reviews_enabled: *reviews_enabled,
                batch_id: batch_id.clone(),
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id: *comment_id,
                check_run_id: *check_run_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
            },
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Commits not in ancestor relationship, continuing review".to_string(),
            }],
        ),

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
            Event::EnableReviewsRequested { head_sha, base_sha },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options: ReviewOptions::default(),
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
                ReviewMachineState::Cancelled { reason, .. } => ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    reason: reason.clone(),
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
    use super::super::state::{BatchId, CheckRunId, CommentId, CommitSha};
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
            comment_id: CommentId(1),
            check_run_id: CheckRunId(2),
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
            comment_id: CommentId(1),
            check_run_id: CheckRunId(2),
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
            comment_id: CommentId(1),
            check_run_id: CheckRunId(2),
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
            comment_id: CommentId(1),
            check_run_id: CheckRunId(2),
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
            comment_id: CommentId(1),
            check_run_id: CheckRunId(2),
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
}

#[cfg(test)]
mod property_tests {
    use super::super::state::{BatchId, CheckRunId, CommentId, CommitSha};
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

    fn arb_check_run_id() -> impl Strategy<Value = CheckRunId> {
        (1u64..1000000).prop_map(CheckRunId::from)
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
            arb_comment_id(),
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
        (any::<bool>(), arb_commit_sha(), arb_cancellation_reason()).prop_map(
            |(reviews_enabled, head_sha, reason)| ReviewMachineState::Cancelled {
                reviews_enabled,
                head_sha,
                reason,
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
            (arb_commit_sha(), arb_commit_sha()).prop_map(|(head_sha, base_sha)| {
                Event::EnableReviewsRequested { head_sha, base_sha }
            }),
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
