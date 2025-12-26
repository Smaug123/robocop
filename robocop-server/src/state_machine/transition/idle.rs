//! Idle state transitions.

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::Event;
use crate::state_machine::state::ReviewMachineState;

/// Handle transitions from the Idle state.
///
/// The Idle state is the initial/rest state. From here, reviews can be
/// triggered by PR updates (if enabled) or explicit requests.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
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

        // Force review on Idle with reviews disabled -> start one-off review
        // (force_review=true overrides reviews_enabled=false)
        (
            ReviewMachineState::Idle {
                reviews_enabled: false,
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
        // Stale Events in Idle State
        // These events are results from effects triggered in a previous state.
        // If we're now Idle, the previous state was cancelled, so ignore these.
        // =====================================================================
        (ReviewMachineState::Idle { .. }, Event::DataFetched { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale DataFetched event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::DataFetchFailed { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale DataFetchFailed event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::BatchSubmitted { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale BatchSubmitted event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::BatchSubmissionFailed { .. }) => {
            TransitionResult::new(
                state.clone(),
                vec![Effect::Log {
                    level: LogLevel::Info,
                    message: "Ignoring stale BatchSubmissionFailed event in Idle state".to_string(),
                }],
            )
        }

        (ReviewMachineState::Idle { .. }, Event::BatchStatusUpdate { .. }) => {
            TransitionResult::new(
                state.clone(),
                vec![Effect::Log {
                    level: LogLevel::Info,
                    message: "Ignoring stale BatchStatusUpdate event in Idle state".to_string(),
                }],
            )
        }

        (ReviewMachineState::Idle { .. }, Event::BatchCompleted { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale BatchCompleted event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::BatchTerminated { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale BatchTerminated event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::AncestryResult { .. }) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale AncestryResult event in Idle state".to_string(),
            }],
        ),

        (ReviewMachineState::Idle { .. }, Event::AncestryCheckFailed { .. }) => {
            TransitionResult::new(
                state.clone(),
                vec![Effect::Log {
                    level: LogLevel::Info,
                    message: "Ignoring stale AncestryCheckFailed event in Idle state".to_string(),
                }],
            )
        }

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

        let result = handle(state, event);

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

        let result = handle(state.clone(), event);

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

        let result = handle(state, event);

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
    fn test_enable_reviews_triggers_new_review() {
        let state = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        let event = Event::EnableReviewsRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = handle(state, event);

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
        let result = handle(state, event);

        // Should preserve reviews_enabled = false even though we're reviewing
        assert!(!result.state.reviews_enabled());
    }
}
