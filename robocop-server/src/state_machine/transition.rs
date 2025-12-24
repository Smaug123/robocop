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
                batch_id,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            },
            vec![], // Comment and check run already created by interpreter
        ),

        // Batch submission failed -> transition to Failed
        (
            ReviewMachineState::Preparing {
                head_sha,
                reviews_enabled,
                ..
            },
            Event::BatchSubmissionFailed { error },
        ) => TransitionResult::new(
            ReviewMachineState::Failed {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                reason: FailureReason::SubmissionFailed { error },
            },
            vec![],
        ),

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
                options: ReviewOptions::default(),
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
}
