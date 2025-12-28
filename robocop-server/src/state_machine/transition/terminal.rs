//! Terminal state transitions (Completed, Failed, Cancelled).

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::Event;
use crate::state_machine::state::ReviewMachineState;

/// Handle transitions from terminal states (Completed, Failed, Cancelled).
///
/// Terminal states are stable resting points. From here, new commits or
/// explicit review requests can trigger new reviews.
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
        // New commit on Cancelled state (enabled, pending batch) -> clear batch, start new review
        (
            ReviewMachineState::Cancelled {
                reviews_enabled: true,
                head_sha: old_head_sha,
                base_sha: old_base_sha,
                pending_cancel_batch_id: Some(_),
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
            vec![
                Effect::ClearBatchSubmission {
                    head_sha: old_head_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // New commit on terminal state (no pending batch) -> start new review if enabled
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
                pending_cancel_batch_id: None,
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

        // Force review on Cancelled state (disabled, pending batch) -> clear batch, start one-off review
        (
            ReviewMachineState::Cancelled {
                reviews_enabled: false,
                head_sha: old_head_sha,
                base_sha: old_base_sha,
                pending_cancel_batch_id: Some(_),
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
            vec![
                Effect::ClearBatchSubmission {
                    head_sha: old_head_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // Force review on terminal state with reviews disabled (no pending batch) -> start one-off review
        // (force_review=true overrides reviews_enabled=false)
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
                pending_cancel_batch_id: None,
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

        // Force review on terminal state -> start new review
        // ReviewRequested on Cancelled state with pending batch -> clear batch, start review
        (
            ReviewMachineState::Cancelled {
                reviews_enabled,
                head_sha: old_head_sha,
                base_sha: old_base_sha,
                pending_cancel_batch_id: Some(_),
                ..
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
            vec![
                Effect::ClearBatchSubmission {
                    head_sha: old_head_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // ReviewRequested on terminal state (no pending batch) -> start new review
        (
            terminal_state @ (ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled {
                pending_cancel_batch_id: None,
                ..
            }),
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

        // Enable reviews on Cancelled state with pending batch -> clear batch, start review
        (
            ReviewMachineState::Cancelled {
                head_sha: old_head_sha,
                base_sha: old_base_sha,
                pending_cancel_batch_id: Some(_),
                ..
            },
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
                Effect::ClearBatchSubmission {
                    head_sha: old_head_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
                Effect::UpdateComment {
                    content: CommentContent::ReviewsEnabled {
                        head_sha: head_sha.clone(),
                    },
                },
                Effect::FetchData { head_sha, base_sha },
            ],
        ),

        // Enable reviews on terminal state (no pending batch) -> mark enabled and start review
        (
            ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled {
                pending_cancel_batch_id: None,
                ..
            },
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
            terminal @ (ReviewMachineState::Completed {
                head_sha, base_sha, ..
            }
            | ReviewMachineState::Failed {
                head_sha, base_sha, ..
            }
            | ReviewMachineState::Cancelled {
                head_sha, base_sha, ..
            }),
            Event::DisableReviewsRequested,
        ) => {
            let new_state = match terminal {
                ReviewMachineState::Completed { result, .. } => ReviewMachineState::Completed {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    result: result.clone(),
                },
                ReviewMachineState::Failed { reason, .. } => ReviewMachineState::Failed {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                    reason: reason.clone(),
                },
                ReviewMachineState::Cancelled {
                    reason,
                    pending_cancel_batch_id,
                    ..
                } => ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
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
                base_sha,
                reason,
                pending_cancel_batch_id: Some(pending_batch_id),
            },
            Event::BatchCompleted { batch_id, .. } | Event::BatchTerminated { batch_id, .. },
        ) if batch_id == *pending_batch_id => TransitionResult::new(
            ReviewMachineState::Cancelled {
                reviews_enabled: *reviews_enabled,
                head_sha: head_sha.clone(),
                base_sha: base_sha.clone(),
                reason: reason.clone(),
                // Clear the tracking - batch is done
                pending_cancel_batch_id: None,
            },
            vec![
                Effect::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "Batch {} completed after cancel was requested, ignoring result",
                        batch_id
                    ),
                },
                // Clear the batch submission cache so re-reviews are possible
                // (retry in case initial clear failed when transitioning to Cancelled)
                Effect::ClearBatchSubmission {
                    head_sha: head_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ],
        ),

        // =====================================================================
        // Stale Events in Terminal States (Completed, Failed, Cancelled)
        // Effect results can arrive after we've reached a terminal state.
        // These are all stale and should be ignored.
        // =====================================================================
        (
            ReviewMachineState::Completed { .. }
            | ReviewMachineState::Failed { .. }
            | ReviewMachineState::Cancelled { .. },
            Event::DataFetched { .. }
            | Event::DataFetchFailed { .. }
            | Event::BatchSubmitted { .. }
            | Event::BatchSubmissionFailed { .. }
            | Event::BatchStatusUpdate { .. }
            | Event::BatchCompleted { .. }
            | Event::BatchTerminated { .. }
            | Event::AncestryResult { .. }
            | Event::AncestryCheckFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale effect result in terminal state".to_string(),
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
        BatchId, CancellationReason, CommitSha, ReviewOptions, ReviewResult,
    };

    #[test]
    fn test_terminal_state_stable_for_same_commit() {
        let state = ReviewMachineState::Completed {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
        };

        // Cancel should do nothing
        let result = handle(state.clone(), Event::CancelRequested);
        assert!(matches!(result.state, ReviewMachineState::Completed { .. }));
        assert!(matches!(
            &result.effects[0],
            Effect::UpdateComment {
                content: CommentContent::NoReviewsToCancel
            }
        ));
    }

    #[test]
    fn test_new_commit_starts_new_review_when_enabled() {
        let state = ReviewMachineState::Completed {
            reviews_enabled: true,
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
        };

        let event = Event::PrUpdated {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("base"),
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
        if let ReviewMachineState::Preparing { head_sha, .. } = &result.state {
            assert_eq!(head_sha.0, "new_sha");
        }
    }

    #[test]
    fn test_new_commit_suppressed_when_disabled() {
        let state = ReviewMachineState::Completed {
            reviews_enabled: false,
            head_sha: CommitSha::from("old_sha"),
            base_sha: CommitSha::from("old_base"),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
        };

        let event = Event::PrUpdated {
            head_sha: CommitSha::from("new_sha"),
            base_sha: CommitSha::from("base"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        let result = handle(state.clone(), event);

        // Should stay in Completed
        assert!(matches!(result.state, ReviewMachineState::Completed { .. }));
        // Should post suppression notice
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::UpdateComment { .. })));
    }

    #[test]
    fn test_enable_reviews_on_terminal_state() {
        let state = ReviewMachineState::Completed {
            reviews_enabled: false,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("old_base"),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "LGTM".to_string(),
            },
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
    }

    #[test]
    fn test_cancelled_with_pending_batch_clears_on_matching_batch_completion() {
        let state = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            reason: CancellationReason::UserRequested,
            pending_cancel_batch_id: Some(BatchId::from("batch_pending".to_string())),
        };

        // Event with matching batch_id should clear the pending_cancel_batch_id
        let event = Event::BatchCompleted {
            batch_id: BatchId::from("batch_pending".to_string()),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "".to_string(),
            },
        };

        let result = handle(state, event);

        // Should stay Cancelled but with pending_cancel_batch_id cleared
        match result.state {
            ReviewMachineState::Cancelled {
                pending_cancel_batch_id,
                ..
            } => {
                assert!(
                    pending_cancel_batch_id.is_none(),
                    "pending_cancel_batch_id should be cleared when batch_id matches"
                );
            }
            other => panic!("expected Cancelled, got {:?}", other),
        }
    }

    /// Regression test: mismatched batch_id should NOT clear pending_cancel_batch_id.
    ///
    /// Bug: Previously, the handler cleared pending_cancel_batch_id without checking
    /// that the incoming batch_id matched. This could incorrectly stop tracking the
    /// real pending-cancel batch if a stale/unrelated batch result event arrived.
    #[test]
    fn test_cancelled_with_pending_batch_ignores_mismatched_batch_completion() {
        let state = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            reason: CancellationReason::UserRequested,
            pending_cancel_batch_id: Some(BatchId::from("batch_pending".to_string())),
        };

        // Event with DIFFERENT batch_id should NOT clear pending_cancel_batch_id
        let event = Event::BatchCompleted {
            batch_id: BatchId::from("stale_batch".to_string()),
            result: ReviewResult {
                reasoning: "".to_string(),
                substantive_comments: false,
                summary: "".to_string(),
            },
        };

        let result = handle(state, event);

        // Should stay Cancelled with pending_cancel_batch_id PRESERVED
        match result.state {
            ReviewMachineState::Cancelled {
                pending_cancel_batch_id,
                ..
            } => {
                assert!(
                    pending_cancel_batch_id.is_some(),
                    "pending_cancel_batch_id should NOT be cleared for mismatched batch_id"
                );
                assert_eq!(
                    pending_cancel_batch_id.unwrap().0,
                    "batch_pending",
                    "pending_cancel_batch_id should remain unchanged"
                );
            }
            other => panic!("expected Cancelled, got {:?}", other),
        }

        // Should log that it's ignoring a stale event
        assert!(result
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Log { .. })));
    }
}
