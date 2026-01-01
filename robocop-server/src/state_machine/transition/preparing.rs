//! Preparing state transitions.

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::{DataFetchFailure, Event};
use crate::state_machine::state::{CancellationReason, FailureReason, ReviewMachineState};

/// Default model for code reviews when none is specified.
const DEFAULT_MODEL: &str = "gpt-5.2-2025-12-11";
/// Default reasoning effort when none is specified.
const DEFAULT_REASONING_EFFORT: &str = "xhigh";

/// Handle transitions from the Preparing state.
///
/// The Preparing state is entered after a review is triggered but before
/// the batch is submitted. We're waiting for diff/file data to be fetched.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
        // Data fetched successfully -> transition to BatchSubmitting and submit batch
        (
            ReviewMachineState::Preparing {
                head_sha,
                base_sha,
                options,
                reviews_enabled,
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

            // Generate reconciliation token for crash recovery
            let reconciliation_token = uuid::Uuid::new_v4().to_string();

            // Extract model and reasoning effort with defaults
            let model = options
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string());
            let reasoning_effort = options
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string());

            // Transition to BatchSubmitting - this state is persisted BEFORE the effect
            // executes, allowing crash recovery via the reconciliation_token.
            TransitionResult::new(
                ReviewMachineState::BatchSubmitting {
                    reviews_enabled: *reviews_enabled,
                    reconciliation_token: reconciliation_token.clone(),
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    options: options.clone(),
                    comment_id: None,   // Will be set by effect result
                    check_run_id: None, // Will be set by effect result
                    model: model.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                },
                vec![Effect::SubmitBatch {
                    diff,
                    file_contents: file_contents_tuples,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    options: options.clone(),
                    reconciliation_token,
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
                        reason: CancellationReason::NoChanges,
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
                        reason: CancellationReason::DiffTooLarge,
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
                        reason: CancellationReason::NoFiles,
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

        // Force review via PrUpdated while preparing (reviews disabled) -> restart with new commit
        // (force_review=true overrides reviews_enabled=false)
        (
            ReviewMachineState::Preparing {
                reviews_enabled: false,
                ..
            },
            Event::PrUpdated {
                head_sha,
                base_sha,
                force_review: true,
                options,
            },
        ) => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: false, // Keep reviews disabled
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
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
        // Stale Events in Preparing State
        // Polling events, batch events, and ancestry events can arrive if a batch
        // was cancelled before its results came back, or from a previous incarnation.
        // BatchSubmitted/BatchSubmissionFailed are now handled by BatchSubmitting state.
        // =====================================================================
        (
            ReviewMachineState::Preparing { .. },
            Event::BatchSubmitted { .. }
            | Event::BatchSubmissionFailed { .. }
            | Event::BatchStatusUpdate { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchTerminated { .. }
            | Event::AncestryResult { .. }
            | Event::AncestryCheckFailed { .. }
            | Event::ReconciliationComplete { .. }
            | Event::ReconciliationFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale batch/polling/ancestry event in Preparing state"
                    .to_string(),
            }],
        ),

        // Catch-all for unhandled events - log warning and return state unchanged
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
    use super::*;
    use crate::state_machine::state::{CommitSha, ReviewOptions};

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

        let result = handle(state, event);

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

    #[test]
    fn test_duplicate_review_request_ignored() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"), // Same SHA
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = handle(state.clone(), event);

        // Should stay in same state
        assert!(matches!(result.state, ReviewMachineState::Preparing { .. }));
        // Should log the duplicate
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Log { .. })));
    }
}
