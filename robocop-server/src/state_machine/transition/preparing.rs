//! Preparing state transitions.

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::{DataFetchFailure, Event};
use crate::state_machine::state::{BatchId, CancellationReason, FailureReason, ReviewMachineState};

/// Handle transitions from the Preparing state.
///
/// The Preparing state is entered after a review is triggered but before
/// the batch is submitted. We're waiting for diff/file data to be fetched.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
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
                base_sha,
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
                        base_sha: base_sha.clone(),
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
                        base_sha: base_sha.clone(),
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
                        base_sha: base_sha.clone(),
                        reason: FailureReason::DataFetchFailed {
                            reason: error.clone(),
                        },
                    },
                    vec![Effect::UpdateComment {
                        content: CommentContent::ReviewFailed {
                            head_sha: head_sha.clone(),
                            batch_id: BatchId::from("(fetch failed)".to_string()),
                            reason: FailureReason::DataFetchFailed { reason: error },
                        },
                    }],
                ),
                DataFetchFailure::NoFiles => (
                    ReviewMachineState::Cancelled {
                        reviews_enabled: *reviews_enabled,
                        head_sha: head_sha.clone(),
                        base_sha: base_sha.clone(),
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
                base_sha,
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
                        batch_id: BatchId::from("(submission failed)".to_string()),
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
                    base_sha: base_sha.clone(),
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

        // Same commit while preparing -> re-drive FetchData
        // This handles the case where we crashed during FetchData on a previous attempt.
        // Instead of ignoring the duplicate request, we restart the fetch.
        // This is also useful for manual retries via `/review` command.
        (
            ReviewMachineState::Preparing {
                head_sha,
                base_sha,
                reviews_enabled,
                ..
            },
            Event::ReviewRequested {
                head_sha: new_head_sha,
                base_sha: new_base_sha,
                options,
            },
        ) if head_sha == &new_head_sha => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                base_sha: new_base_sha.clone(),
                options,
            },
            vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Re-driving FetchData for commit {} (duplicate ReviewRequested, may be recovery)",
                        head_sha.short()
                    ),
                },
                Effect::FetchData {
                    head_sha: head_sha.clone(),
                    base_sha: new_base_sha,
                },
            ],
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

        // Same (head_sha, base_sha) while preparing -> no change (duplicate webhook)
        (
            ReviewMachineState::Preparing {
                head_sha, base_sha, ..
            },
            Event::PrUpdated {
                head_sha: new_head_sha,
                base_sha: new_base_sha,
                ..
            },
        ) if head_sha == &new_head_sha && base_sha == &new_base_sha => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Ignoring duplicate PrUpdated for same commit {} and base {}",
                    head_sha.short(),
                    base_sha.short()
                ),
            }],
        ),

        // Same head_sha but different base_sha while preparing -> restart with new base
        // This handles the case where the target branch moved (e.g., another PR merged)
        (
            ReviewMachineState::Preparing {
                head_sha,
                reviews_enabled,
                ..
            },
            Event::PrUpdated {
                head_sha: new_head_sha,
                base_sha: new_base_sha,
                options,
                ..
            },
        ) if head_sha == &new_head_sha && *reviews_enabled => TransitionResult::new(
            ReviewMachineState::Preparing {
                reviews_enabled: true,
                head_sha: head_sha.clone(),
                base_sha: new_base_sha.clone(),
                options,
            },
            vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Re-driving FetchData for commit {} (base branch moved to {})",
                        head_sha.short(),
                        new_base_sha.short()
                    ),
                },
                Effect::FetchData {
                    head_sha: head_sha.clone(),
                    base_sha: new_base_sha,
                },
            ],
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
        // Polling events and ancestry events can arrive if a batch was cancelled
        // before its results came back, or from a previous incarnation. Ignore them.
        // =====================================================================
        (
            ReviewMachineState::Preparing { .. },
            Event::BatchStatusUpdate { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchTerminated { .. }
            | Event::AncestryResult { .. }
            | Event::AncestryCheckFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale polling/ancestry event in Preparing state".to_string(),
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

    /// Same-SHA `ReviewRequested` re-drives `FetchData` for crash recovery.
    /// This is useful when the server crashed during a previous FetchData attempt,
    /// or when a user explicitly requests a retry via `/review` command.
    #[test]
    fn test_duplicate_review_request_redrives_fetch() {
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

        // Should stay in Preparing state
        assert!(matches!(result.state, ReviewMachineState::Preparing { .. }));

        // Should log the re-drive
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log { level: LogLevel::Info, message }
                    if message.contains("Re-driving FetchData")
            )),
            "Should log re-driving FetchData"
        );

        // Should emit FetchData effect for crash recovery
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { head_sha, base_sha }
                    if head_sha.0 == "abc123" && base_sha.0 == "def456"
            )),
            "Should emit FetchData effect for recovery"
        );
    }

    /// Regression test: When re-driving FetchData for the same head_sha but different
    /// base_sha, both the state and the FetchData effect must use the NEW base_sha.
    /// Previously, the state was updated correctly but the effect used the old base_sha,
    /// causing the diff to be fetched for the wrong merge-base.
    #[test]
    fn test_same_head_different_base_uses_new_base_in_effect() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("old_base"),
            options: ReviewOptions::default(),
        };

        // Same head_sha but different base_sha (base branch moved)
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("new_base"),
            options: ReviewOptions::default(),
        };

        let result = handle(state, event);

        // Should stay in Preparing with the NEW base_sha
        if let ReviewMachineState::Preparing { base_sha, .. } = &result.state {
            assert_eq!(base_sha.0, "new_base", "State should use new base_sha");
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }

        // FetchData effect must use the NEW base_sha (not the old one!)
        let fetch_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::FetchData { .. }));
        assert!(fetch_effect.is_some(), "Should emit FetchData effect");
        if let Some(Effect::FetchData { base_sha, .. }) = fetch_effect {
            assert_eq!(
                base_sha.0, "new_base",
                "FetchData effect must use new base_sha, not old"
            );
        }
    }

    /// Test that PrUpdated with same head_sha but different base_sha restarts the review.
    /// This matches the behavior for ReviewRequested with same head/different base.
    #[test]
    fn test_pr_updated_same_head_different_base_restarts() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("old_base"),
            options: ReviewOptions::default(),
        };

        // Same head_sha but different base_sha (target branch moved)
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("new_base"),
            options: ReviewOptions::default(),
            force_review: false,
        };

        let result = handle(state, event);

        // Should stay in Preparing with the NEW base_sha
        if let ReviewMachineState::Preparing { base_sha, .. } = &result.state {
            assert_eq!(base_sha.0, "new_base", "State should use new base_sha");
        } else {
            panic!("Expected Preparing state, got {:?}", result.state);
        }

        // Should log the base branch move
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log { level: LogLevel::Info, message }
                    if message.contains("base branch moved")
            )),
            "Should log base branch movement"
        );

        // FetchData effect must use the NEW base_sha
        let fetch_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::FetchData { .. }));
        assert!(fetch_effect.is_some(), "Should emit FetchData effect");
        if let Some(Effect::FetchData { base_sha, .. }) = fetch_effect {
            assert_eq!(
                base_sha.0, "new_base",
                "FetchData effect must use new base_sha"
            );
        }
    }

    /// Test that duplicate PrUpdated (same head AND base) is ignored.
    #[test]
    fn test_pr_updated_duplicate_ignored() {
        let state = ReviewMachineState::Preparing {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"), // Same head
            base_sha: CommitSha::from("def456"), // Same base
            options: ReviewOptions::default(),
            force_review: false,
        };

        let result = handle(state.clone(), event);

        // Should stay in same state (no change)
        assert!(matches!(result.state, ReviewMachineState::Preparing { .. }));

        // Should log the duplicate (not emit FetchData)
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::Log { level: LogLevel::Info, message }
                    if message.contains("Ignoring duplicate PrUpdated")
            )),
            "Should log ignoring duplicate"
        );

        // Should NOT emit FetchData
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::FetchData { .. })),
            "Should NOT emit FetchData for duplicate"
        );
    }
}
