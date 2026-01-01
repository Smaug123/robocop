//! BatchSubmitting state transitions.
//!
//! This state exists for crash recovery. When we transition from Preparing to BatchSubmitting,
//! we persist the state BEFORE executing the SubmitBatch effect. If a crash occurs after the
//! batch is submitted to OpenAI but before we process the BatchSubmitted event, we can recover
//! by listing batches at OpenAI and matching the reconciliation_token.

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::Event;
use crate::state_machine::state::{BatchId, FailureReason, ReviewMachineState};

/// Handle transitions from the BatchSubmitting state.
///
/// This state is entered after data is fetched but before the batch is submitted.
/// We generate a reconciliation_token, persist this state, then execute SubmitBatch.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
        // Batch submitted successfully -> transition to BatchPending
        (
            ReviewMachineState::BatchSubmitting {
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
                    batch_id,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    comment_id,
                    check_run_id,
                    model,
                    reasoning_effort,
                },
                effects,
            )
        }

        // Batch submission failed -> transition to Failed
        (
            ReviewMachineState::BatchSubmitting {
                head_sha,
                reviews_enabled,
                check_run_id,
                ..
            },
            Event::BatchSubmissionFailed {
                error,
                comment_id: _,
                check_run_id: event_check_run_id,
            },
        ) => {
            // Use check_run_id from event if available, otherwise from state
            let effective_check_run_id = event_check_run_id.or(*check_run_id);

            let mut effects = vec![Effect::UpdateComment {
                content: CommentContent::ReviewFailed {
                    head_sha: head_sha.clone(),
                    batch_id: BatchId::from("(submission failed)".to_string()),
                    reason: FailureReason::SubmissionFailed {
                        error: error.clone(),
                    },
                },
            }];

            if let Some(cr_id) = effective_check_run_id {
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

        // Reconciliation found a batch at OpenAI -> transition to BatchPending
        // This is the crash recovery path.
        (
            ReviewMachineState::BatchSubmitting {
                head_sha,
                base_sha,
                reviews_enabled,
                comment_id: state_comment_id,
                check_run_id: state_check_run_id,
                ..
            },
            Event::ReconciliationComplete {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            },
        ) => {
            // Use comment_id and check_run_id from the event if available,
            // otherwise fall back to any we might have from state (though typically None)
            let effective_comment_id = comment_id.or(*state_comment_id);
            let effective_check_run_id = check_run_id.or(*state_check_run_id);

            // Emit UI updates like the normal BatchSubmitted path does
            let mut effects = vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: "Recovered from crash - found batch at OpenAI".to_string(),
                },
                Effect::UpdateComment {
                    content: CommentContent::InProgress {
                        head_sha: head_sha.clone(),
                        batch_id: batch_id.clone(),
                        model: model.clone(),
                        reasoning_effort: reasoning_effort.clone(),
                    },
                },
            ];

            // Update check run with batch ID as external_id for correlation
            if let Some(cr_id) = effective_check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: cr_id,
                    status: EffectCheckRunStatus::InProgress,
                    conclusion: None,
                    title: "Code review in progress".to_string(),
                    summary: format!(
                        "Reviewing commit {} with {} (batch: {}) [recovered]",
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
                    batch_id,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    comment_id: effective_comment_id,
                    check_run_id: effective_check_run_id,
                    model,
                    reasoning_effort,
                },
                effects,
            )
        }

        // Reconciliation failed to find a batch -> transition back to Preparing to retry
        // The batch was never submitted (crash happened before OpenAI call).
        (
            ReviewMachineState::BatchSubmitting {
                head_sha,
                base_sha,
                options,
                reviews_enabled,
                ..
            },
            Event::ReconciliationFailed {
                reconciliation_token,
                error,
            },
        ) => {
            // Go back to Preparing state to re-fetch and retry
            TransitionResult::new(
                ReviewMachineState::Preparing {
                    reviews_enabled: *reviews_enabled,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    options: options.clone(),
                },
                vec![
                    Effect::Log {
                        level: LogLevel::Warn,
                        message: format!(
                            "Reconciliation failed for token {}: {}. Retrying from Preparing.",
                            reconciliation_token, error
                        ),
                    },
                    Effect::FetchData {
                        head_sha: head_sha.clone(),
                        base_sha: base_sha.clone(),
                    },
                ],
            )
        }

        // Cancel while submitting -> go back to idle
        (
            ReviewMachineState::BatchSubmitting {
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

        // Disable reviews while submitting -> go to Idle with reviews disabled
        (ReviewMachineState::BatchSubmitting { .. }, Event::DisableReviewsRequested) => {
            TransitionResult::new(
                ReviewMachineState::Idle {
                    reviews_enabled: false,
                },
                vec![Effect::UpdateComment {
                    content: CommentContent::ReviewsDisabled { cancelled_count: 0 },
                }],
            )
        }

        // PrUpdated while submitting -> restart with new commit
        (
            ReviewMachineState::BatchSubmitting {
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

        // PrUpdated while submitting (reviews disabled) -> go to Idle
        (
            ReviewMachineState::BatchSubmitting {
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

        // Force review via PrUpdated while submitting (reviews disabled) -> restart
        (
            ReviewMachineState::BatchSubmitting {
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
                reviews_enabled: false,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                options,
            },
            vec![Effect::FetchData { head_sha, base_sha }],
        ),

        // =====================================================================
        // Stale Events in BatchSubmitting State
        // =====================================================================
        (
            ReviewMachineState::BatchSubmitting { .. },
            Event::BatchStatusUpdate { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchTerminated { .. }
            | Event::AncestryResult { .. }
            | Event::AncestryCheckFailed { .. }
            | Event::DataFetched { .. }
            | Event::DataFetchFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale event in BatchSubmitting state".to_string(),
            }],
        ),

        // Catch-all for unhandled events
        (_, event) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Warn,
                message: format!("Unhandled event {:?} in BatchSubmitting state", event),
            }],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::{BatchId, CheckRunId, CommentId, CommitSha, ReviewOptions};

    fn make_batch_submitting_state() -> ReviewMachineState {
        ReviewMachineState::BatchSubmitting {
            reviews_enabled: true,
            reconciliation_token: "test-token-123".to_string(),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        }
    }

    #[test]
    fn test_batch_submitted_transitions_to_pending() {
        let state = make_batch_submitting_state();
        let event = Event::BatchSubmitted {
            batch_id: BatchId::from("batch_123".to_string()),
            comment_id: Some(CommentId(42)),
            check_run_id: Some(CheckRunId(99)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = handle(state, event);

        match &result.state {
            ReviewMachineState::BatchPending {
                batch_id,
                comment_id,
                check_run_id,
                ..
            } => {
                assert_eq!(batch_id.0, "batch_123");
                assert_eq!(comment_id.unwrap().0, 42);
                assert_eq!(check_run_id.unwrap().0, 99);
            }
            _ => panic!("Expected BatchPending, got {:?}", result.state),
        }
    }

    #[test]
    fn test_batch_submission_failed_transitions_to_failed() {
        let state = make_batch_submitting_state();
        let event = Event::BatchSubmissionFailed {
            error: "API error".to_string(),
            comment_id: None,
            check_run_id: None,
        };

        let result = handle(state, event);

        match &result.state {
            ReviewMachineState::Failed { reason, .. } => {
                assert!(matches!(reason, FailureReason::SubmissionFailed { .. }));
            }
            _ => panic!("Expected Failed, got {:?}", result.state),
        }
    }

    #[test]
    fn test_reconciliation_complete_transitions_to_pending() {
        let state = make_batch_submitting_state();
        let event = Event::ReconciliationComplete {
            batch_id: BatchId::from("recovered_batch".to_string()),
            comment_id: Some(CommentId(100)),
            check_run_id: Some(CheckRunId(200)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = handle(state, event);

        match &result.state {
            ReviewMachineState::BatchPending { batch_id, .. } => {
                assert_eq!(batch_id.0, "recovered_batch");
            }
            _ => panic!("Expected BatchPending, got {:?}", result.state),
        }
    }

    #[test]
    fn test_reconciliation_failed_goes_back_to_preparing() {
        let state = make_batch_submitting_state();
        let event = Event::ReconciliationFailed {
            reconciliation_token: "test-token-123".to_string(),
            error: "No matching batch found".to_string(),
        };

        let result = handle(state, event);

        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Expected Preparing, got {:?}",
            result.state
        );

        // Should emit FetchData to retry
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::FetchData { .. })));
    }

    /// ReconciliationComplete should emit UpdateComment and UpdateCheckRun effects
    /// to update the UI with the recovered batch ID, just like BatchSubmitted does.
    #[test]
    fn test_reconciliation_complete_emits_ui_updates() {
        let state = make_batch_submitting_state();
        let event = Event::ReconciliationComplete {
            batch_id: BatchId::from("recovered_batch".to_string()),
            comment_id: Some(CommentId(100)),
            check_run_id: Some(CheckRunId(200)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = handle(state, event);

        // Should transition to BatchPending
        assert!(
            matches!(result.state, ReviewMachineState::BatchPending { .. }),
            "Expected BatchPending, got {:?}",
            result.state
        );

        // Should emit UpdateComment effect to show batch ID in the comment
        let has_update_comment = result.effects.iter().any(|e| {
            matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::InProgress { .. }
                }
            )
        });
        assert!(
            has_update_comment,
            "ReconciliationComplete should emit UpdateComment with InProgress content. Effects: {:?}",
            result.effects
        );

        // Should emit UpdateCheckRun effect to set external_id to batch ID
        let has_update_check_run = result.effects.iter().any(|e| {
            matches!(
                e,
                Effect::UpdateCheckRun {
                    external_id: Some(_),
                    ..
                }
            )
        });
        assert!(
            has_update_check_run,
            "ReconciliationComplete should emit UpdateCheckRun with external_id. Effects: {:?}",
            result.effects
        );
    }

    /// ReconciliationComplete without check_run_id should still emit UpdateComment
    #[test]
    fn test_reconciliation_complete_without_check_run() {
        let state = make_batch_submitting_state();
        let event = Event::ReconciliationComplete {
            batch_id: BatchId::from("recovered_batch".to_string()),
            comment_id: Some(CommentId(100)),
            check_run_id: None, // No check run
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let result = handle(state, event);

        // Should still emit UpdateComment
        let has_update_comment = result.effects.iter().any(|e| {
            matches!(
                e,
                Effect::UpdateComment {
                    content: CommentContent::InProgress { .. }
                }
            )
        });
        assert!(
            has_update_comment,
            "ReconciliationComplete should emit UpdateComment even without check_run_id. Effects: {:?}",
            result.effects
        );
    }
}
