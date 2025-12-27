//! BatchPending state transitions.

use super::{sanitize_check_run_title, TransitionResult};
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::Event;
use crate::state_machine::state::{CancellationReason, FailureReason, ReviewMachineState};

/// Handle transitions from the BatchPending state.
///
/// The BatchPending state is active while a review batch is being processed
/// by OpenAI. We're waiting for the batch to complete or fail.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
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

        // Batch status update (terminal status) -> no state change, wait for result event
        // The polling loop noticed the batch finished, but we'll get the actual results
        // via BatchCompleted or BatchTerminated events.
        (
            ReviewMachineState::BatchPending { batch_id, .. },
            Event::BatchStatusUpdate {
                batch_id: event_batch_id,
                status,
            },
        ) if batch_id == &event_batch_id && status.is_terminal() => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Batch {} has terminal status {:?}, waiting for result event",
                    batch_id, status
                ),
            }],
        ),

        // Batch completed -> transition to Completed
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                base_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::BatchCompleted {
                batch_id: event_batch_id,
                result,
            },
        ) if batch_id == &event_batch_id => {
            let conclusion = if result.substantive_comments {
                EffectCheckRunConclusion::Failure
            } else {
                EffectCheckRunConclusion::Success
            };
            let (title, result_summary) = if result.substantive_comments {
                (
                    sanitize_check_run_title(&format!(
                        "Code review found issues: {}",
                        result.summary
                    )),
                    result.summary.clone(),
                )
            } else {
                ("Code review passed".to_string(), result.summary.clone())
            };

            let new_state = ReviewMachineState::Completed {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                result: result.clone(),
            };

            let mut effects = vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewComplete {
                        head_sha: head_sha.clone(),
                        batch_id: batch_id.clone(),
                        result,
                    },
                },
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ];

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
                base_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::BatchTerminated {
                batch_id: event_batch_id,
                reason: FailureReason::BatchCancelled,
            },
        ) if batch_id == &event_batch_id => {
            let mut effects = vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewCancelled {
                        head_sha: head_sha.clone(),
                        reason: CancellationReason::External, // Batch was cancelled externally
                    },
                },
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ];

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
                    base_sha: base_sha.clone(),
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
                base_sha,
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
                base_sha: base_sha.clone(),
                reason: reason.clone(),
            };

            let mut effects = vec![
                Effect::UpdateComment {
                    content: CommentContent::ReviewFailed {
                        head_sha: head_sha.clone(),
                        batch_id: batch_id.clone(),
                        reason,
                    },
                },
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ];

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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
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
                    base_sha: base_sha.clone(),
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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
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
                    base_sha: base_sha.clone(),
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

        // ReviewRequested for SAME (head_sha, base_sha) while batch pending -> no-op (de-dup)
        // Note: We must check BOTH head_sha AND base_sha because the diff depends on both.
        // If the base branch moved, the diff is different and we need a new review.
        (
            ReviewMachineState::BatchPending {
                head_sha: pending_head,
                base_sha: pending_base,
                ..
            },
            Event::ReviewRequested {
                head_sha: requested_head,
                base_sha: requested_base,
                ..
            },
        ) if pending_head == &requested_head && pending_base == &requested_base => {
            TransitionResult::new(
                state.clone(),
                vec![Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Review already pending for commit {} (base {}), ignoring duplicate request",
                        pending_head.short(),
                        pending_base.short()
                    ),
                }],
            )
        }

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

        // Same head_sha and base_sha while batch pending -> no change (duplicate webhook)
        (
            ReviewMachineState::BatchPending {
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
                    "Ignoring duplicate PrUpdated for same commit {}",
                    head_sha.short()
                ),
            }],
        ),

        // Same head_sha but different base_sha -> base branch moved, restart review
        // This can happen when main gets new commits while a review is pending.
        // The diff changes (since it's relative to a new base), so we need a new review.
        (
            ReviewMachineState::BatchPending {
                batch_id,
                head_sha,
                base_sha,
                check_run_id,
                reviews_enabled,
                ..
            },
            Event::PrUpdated {
                head_sha: new_head_sha,
                base_sha: new_base_sha,
                options,
                ..
            },
        ) if head_sha == &new_head_sha && base_sha != &new_base_sha => {
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Base branch moved from {} to {} for commit {}, restarting review",
                        base_sha.short(),
                        new_base_sha.short(),
                        head_sha.short()
                    ),
                },
                // Clear the old batch submission cache entry so the new review can proceed
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Base branch moved to {}", new_base_sha.short()),
                    summary: "The base branch moved, so a new review is starting.".to_string(),
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
                    head_sha: new_head_sha,
                    base_sha: new_base_sha,
                    options,
                },
                effects,
            )
        }

        // =====================================================================
        // Stale Events in BatchPending State
        // Data fetch events are from a previous incarnation; ancestry events
        // are only valid in AwaitingAncestryCheck. Ignore them.
        // =====================================================================
        (
            ReviewMachineState::BatchPending { .. },
            Event::DataFetched { .. }
            | Event::DataFetchFailed { .. }
            | Event::BatchSubmitted { .. }
            | Event::BatchSubmissionFailed { .. }
            | Event::AncestryResult { .. }
            | Event::AncestryCheckFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale event in BatchPending state".to_string(),
            }],
        ),

        // Mismatched batch polling events (batch_id doesn't match current batch)
        // This can happen if a previous batch's results arrive after we've started a new one.
        (
            ReviewMachineState::BatchPending { batch_id, .. },
            Event::BatchStatusUpdate {
                batch_id: event_batch_id,
                ..
            }
            | Event::BatchCompleted {
                batch_id: event_batch_id,
                ..
            }
            | Event::BatchTerminated {
                batch_id: event_batch_id,
                ..
            },
        ) if batch_id != &event_batch_id => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: format!(
                    "Ignoring polling event for stale batch {} (current: {})",
                    event_batch_id, batch_id
                ),
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
    use crate::state_machine::state::{
        BatchId, CheckRunId, CommentId, CommitSha, ReviewOptions, ReviewResult,
    };

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
            result: ReviewResult {
                reasoning: "Code looks good".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
        };

        let result = handle(state, event);

        assert!(matches!(result.state, ReviewMachineState::Completed { .. }));
        // Effects: UpdateComment, ClearBatchSubmission, UpdateCheckRun
        assert_eq!(result.effects.len(), 3);
        assert!(matches!(
            &result.effects[0],
            Effect::UpdateComment {
                content: CommentContent::ReviewComplete { .. }
            }
        ));
        assert!(matches!(
            &result.effects[1],
            Effect::ClearBatchSubmission { .. }
        ));
        assert!(matches!(&result.effects[2], Effect::UpdateCheckRun { .. }));
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

        let result = handle(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::Cancelled {
                reason: CancellationReason::UserRequested,
                ..
            }
        ));
        // Effects: CancelBatch, UpdateComment, ClearBatchSubmission, UpdateCheckRun
        assert_eq!(result.effects.len(), 4);
        assert!(matches!(&result.effects[0], Effect::CancelBatch { .. }));
        assert!(matches!(
            &result.effects[2],
            Effect::ClearBatchSubmission { .. }
        ));
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

        let result = handle(state, event);

        assert!(matches!(
            result.state,
            ReviewMachineState::AwaitingAncestryCheck { .. }
        ));
        assert_eq!(result.effects.len(), 1);
        assert!(matches!(&result.effects[0], Effect::CheckAncestry { .. }));
    }

    #[test]
    fn test_base_sha_change_restarts_review() {
        // When head_sha is the same but base_sha changes (base branch moved),
        // we should restart the review because the diff is now different.
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"),   // Same head_sha
            base_sha: CommitSha::from("new_base"), // Different base_sha
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = handle(state, event);

        // Should transition to Preparing for the new base
        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Expected Preparing state, got {:?}",
            result.state
        );
        if let ReviewMachineState::Preparing {
            head_sha, base_sha, ..
        } = &result.state
        {
            assert_eq!(head_sha.0, "abc123", "Should keep same head_sha");
            assert_eq!(base_sha.0, "new_base", "Should use new base_sha");
        }

        // Should cancel the old batch
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should cancel the old batch"
        );

        // Should clear the batch submission cache
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::ClearBatchSubmission { .. })),
            "Should clear the batch submission cache"
        );

        // Should fetch data for the new base
        assert!(
            result.effects.iter().any(|e| matches!(
                e,
                Effect::FetchData { base_sha, .. } if base_sha.0 == "new_base"
            )),
            "Should fetch data with new base_sha"
        );
    }

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
                reasoning_effort: None,
            },
        };

        let result = handle(state, event);

        // Should transition to Preparing for the new commit
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

    /// Regression test: ReviewRequested with same head_sha but different base_sha
    /// should NOT be treated as a duplicate. The diff depends on both head and base,
    /// so if the base moved, we need a new review.
    #[test]
    fn test_review_requested_same_head_different_base_not_duplicate() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId::from("batch_123".to_string()),
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("old_base"),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".to_string(),
            reasoning_effort: "medium".to_string(),
        };

        // Same head_sha but different base_sha (base branch moved)
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("new_base"),
            options: ReviewOptions::default(),
        };

        let result = handle(state, event);

        // Should NOT stay in BatchPending - should cancel and restart
        assert!(
            matches!(result.state, ReviewMachineState::Preparing { .. }),
            "Should transition to Preparing, not ignore as duplicate. Got: {:?}",
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

        // Should fetch data with the NEW base_sha
        let fetch_effect = result
            .effects
            .iter()
            .find(|e| matches!(e, Effect::FetchData { .. }));
        assert!(fetch_effect.is_some(), "Should emit FetchData effect");
        if let Some(Effect::FetchData {
            head_sha, base_sha, ..
        }) = fetch_effect
        {
            assert_eq!(head_sha.0, "abc123", "Should use same head_sha");
            assert_eq!(base_sha.0, "new_base", "Should use new base_sha");
        }
    }

    /// Test that exact duplicate (same head AND base) IS ignored
    #[test]
    fn test_review_requested_exact_duplicate_ignored() {
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

        // Exact same head_sha AND base_sha
        let event = Event::ReviewRequested {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            options: ReviewOptions::default(),
        };

        let result = handle(state.clone(), event);

        // Should stay in BatchPending (duplicate)
        assert!(
            matches!(result.state, ReviewMachineState::BatchPending { .. }),
            "Should stay in BatchPending for exact duplicate"
        );

        // Should NOT cancel or fetch
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::CancelBatch { .. })),
            "Should NOT cancel batch for duplicate"
        );
        assert!(
            !result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::FetchData { .. })),
            "Should NOT fetch data for duplicate"
        );

        // Should log
        assert!(
            result
                .effects
                .iter()
                .any(|e| matches!(e, Effect::Log { .. })),
            "Should log duplicate request"
        );
    }
}
