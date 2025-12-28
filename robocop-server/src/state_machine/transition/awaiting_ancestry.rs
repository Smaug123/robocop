//! AwaitingAncestryCheck state transitions.

use super::TransitionResult;
use crate::state_machine::effect::{
    CommentContent, Effect, EffectCheckRunConclusion, EffectCheckRunStatus, LogLevel,
};
use crate::state_machine::event::Event;
use crate::state_machine::state::{CancellationReason, ReviewMachineState};

/// Handle transitions from the AwaitingAncestryCheck state.
///
/// This state is entered when a new commit arrives while a batch is pending.
/// We're waiting to determine if the new commit supersedes the old one (is an
/// ancestor) or if they diverged (e.g., force-push/rebase).
pub fn handle(state: ReviewMachineState, event: Event) -> TransitionResult {
    match (&state, event) {
        // Old commit is superseded and reviews are enabled -> cancel old, start new review
        (
            ReviewMachineState::AwaitingAncestryCheck {
                batch_id,
                head_sha: old_sha,
                base_sha: old_base_sha,
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
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: old_base_sha.clone(),
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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: base_sha.clone(),
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
                    base_sha: base_sha.clone(),
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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: base_sha.clone(),
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
                    base_sha: base_sha.clone(),
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
                base_sha: old_base_sha,
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
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: old_base_sha.clone(),
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
                base_sha: old_base_sha,
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
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: old_base_sha.clone(),
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
                base_sha: old_base_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::BatchCompleted { result, .. },
        ) => {
            let mut effects = vec![
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
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
                        "Review completed but superseded by newer commit: {}. Result was: {} - {}",
                        new_head_sha.short(),
                        if result.substantive_comments {
                            "Issues found"
                        } else {
                            "No issues"
                        },
                        result.summary
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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: base_sha.clone(),
                },
            ];

            if let Some(cr_id) = check_run_id {
                effects.push(Effect::UpdateCheckRun {
                    check_run_id: *cr_id,
                    status: EffectCheckRunStatus::Completed,
                    conclusion: Some(EffectCheckRunConclusion::Stale),
                    title: format!("Superseded by {}", new_head_sha.short()),
                    summary: format!(
                        "Review completed but superseded by newer commit: {} (reviews disabled). Result was: {} - {}",
                        new_head_sha.short(),
                        if result.substantive_comments { "Issues found" } else { "No issues" },
                        result.summary
                    ),
                    external_id: None,
                });
            }

            TransitionResult::new(
                ReviewMachineState::Cancelled {
                    reviews_enabled: false,
                    head_sha: old_sha.clone(),
                    base_sha: base_sha.clone(),
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
                base_sha: old_base_sha,
                check_run_id,
                new_head_sha,
                new_base_sha,
                new_options,
                ..
            },
            Event::BatchTerminated { reason, .. },
        ) => {
            let mut effects = vec![
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
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
                base_sha,
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
                // Clear the batch submission cache so re-reviews are possible
                Effect::ClearBatchSubmission {
                    head_sha: old_sha.clone(),
                    base_sha: base_sha.clone(),
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
                    base_sha: base_sha.clone(),
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
                head_sha: old_head_sha,
                base_sha: old_base_sha,
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
            let mut effects = vec![
                Effect::CancelBatch {
                    batch_id: batch_id.clone(),
                },
                Effect::ClearBatchSubmission {
                    head_sha: old_head_sha.clone(),
                    base_sha: old_base_sha.clone(),
                },
            ];

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

        // =====================================================================
        // Stale Events in AwaitingAncestryCheck State
        // Data fetch and batch submission events are from a previous incarnation.
        // Ignore them.
        // =====================================================================
        (
            ReviewMachineState::AwaitingAncestryCheck { .. },
            Event::DataFetched { .. }
            | Event::DataFetchFailed { .. }
            | Event::BatchSubmitted { .. }
            | Event::BatchSubmissionFailed { .. },
        ) => TransitionResult::new(
            state.clone(),
            vec![Effect::Log {
                level: LogLevel::Info,
                message: "Ignoring stale event in AwaitingAncestryCheck state".to_string(),
            }],
        ),

        // Batch status update while awaiting ancestry check (still processing) -> no change
        // The batch is still running, we're just waiting for either the ancestry
        // result or the batch to complete.
        (
            ReviewMachineState::AwaitingAncestryCheck { batch_id, .. },
            Event::BatchStatusUpdate {
                batch_id: event_batch_id,
                status,
            },
        ) if batch_id == &event_batch_id && status.is_processing() => {
            TransitionResult::no_change(state.clone())
        }

        // Batch status update while awaiting ancestry check (terminal status) -> no change
        // The polling loop noticed the batch finished, but we'll get the actual results
        // via BatchCompleted or BatchTerminated events. Just acknowledge and wait.
        (
            ReviewMachineState::AwaitingAncestryCheck { batch_id, .. },
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

        // Mismatched batch polling events in AwaitingAncestryCheck
        // (batch_id doesn't match current batch)
        (
            ReviewMachineState::AwaitingAncestryCheck { batch_id, .. },
            Event::BatchStatusUpdate {
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
    use crate::state_machine::state::{BatchId, CheckRunId, CommentId, CommitSha, ReviewOptions};

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

        let result = handle(state, event);

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
    fn test_superseded_commit_preserves_new_options() {
        // New commit pushed with custom options (model and reasoning_effort)
        let custom_options = ReviewOptions {
            model: Some("o3".to_string()),
            reasoning_effort: Some("high".to_string()),
        };

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
            new_base_sha: CommitSha::from("base_sha"),
            new_options: custom_options,
        };

        let event = Event::AncestryResult {
            old_sha: CommitSha::from("old_sha"),
            new_sha: CommitSha::from("new_sha"),
            is_superseded: true,
        };

        let result = handle(state, event);

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

        let result = handle(state, event);

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
}
