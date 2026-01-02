//! Status types for the status endpoint.
//!
//! This module provides types for displaying the bot's state in a web browser.

use serde::Serialize;

use crate::state_machine::repository::StoredState;
use crate::state_machine::state::ReviewMachineState;
use crate::state_machine::store::StateMachinePrId;

/// Summary statistics for the status page.
#[derive(Debug, Default, Serialize)]
pub struct StatusSummary {
    pub total_prs: usize,
    pub idle: usize,
    pub preparing: usize,
    pub batch_submitting: usize,
    pub awaiting_ancestry_check: usize,
    pub batch_pending: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

/// A PR entry for display on the status page.
#[derive(Debug, Serialize)]
pub struct PrStatusEntry {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub state_name: String,
    pub head_sha: Option<String>,
    pub batch_id: Option<String>,
    pub reviews_enabled: bool,
    pub model: Option<String>,
    pub failure_reason: Option<String>,
    pub cancellation_reason: Option<String>,
}

/// Full status data for rendering.
#[derive(Debug, Serialize)]
pub struct StatusData {
    pub version: String,
    pub summary: StatusSummary,
    pub prs: Vec<PrStatusEntry>,
}

impl StatusData {
    /// Create status data from a list of stored states.
    pub fn from_states(states: Vec<(StateMachinePrId, StoredState)>, version: String) -> Self {
        let mut summary = StatusSummary {
            total_prs: states.len(),
            ..Default::default()
        };

        let mut prs = Vec::with_capacity(states.len());

        for (id, stored) in states {
            let info = extract_state_info(&stored.state);

            // Update summary counts
            match &stored.state {
                ReviewMachineState::Idle { .. } => summary.idle += 1,
                ReviewMachineState::Preparing { .. } => summary.preparing += 1,
                ReviewMachineState::BatchSubmitting { .. } => summary.batch_submitting += 1,
                ReviewMachineState::AwaitingAncestryCheck { .. } => {
                    summary.awaiting_ancestry_check += 1
                }
                ReviewMachineState::BatchPending { .. } => summary.batch_pending += 1,
                ReviewMachineState::Completed { .. } => summary.completed += 1,
                ReviewMachineState::Failed { .. } => summary.failed += 1,
                ReviewMachineState::Cancelled { .. } => summary.cancelled += 1,
            }

            prs.push(PrStatusEntry {
                repo_owner: id.repo_owner,
                repo_name: id.repo_name,
                pr_number: id.pr_number,
                state_name: info.state_name,
                head_sha: info.head_sha,
                batch_id: info.batch_id,
                reviews_enabled: stored.state.reviews_enabled(),
                model: info.model,
                failure_reason: info.failure_reason,
                cancellation_reason: info.cancellation_reason,
            });
        }

        Self {
            version,
            summary,
            prs,
        }
    }
}

/// Extracted display information from a state.
struct ExtractedStateInfo {
    state_name: String,
    head_sha: Option<String>,
    batch_id: Option<String>,
    model: Option<String>,
    failure_reason: Option<String>,
    cancellation_reason: Option<String>,
}

/// Extract display information from a state.
fn extract_state_info(state: &ReviewMachineState) -> ExtractedStateInfo {
    match state {
        ReviewMachineState::Idle { .. } => ExtractedStateInfo {
            state_name: "Idle".to_string(),
            head_sha: None,
            batch_id: None,
            model: None,
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::Preparing { head_sha, .. } => ExtractedStateInfo {
            state_name: "Preparing".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: None,
            model: None,
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::BatchSubmitting {
            head_sha, model, ..
        } => ExtractedStateInfo {
            state_name: "BatchSubmitting".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: None,
            model: Some(model.clone()),
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::AwaitingAncestryCheck {
            head_sha,
            batch_id,
            model,
            ..
        } => ExtractedStateInfo {
            state_name: "AwaitingAncestryCheck".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: Some(batch_id.0.clone()),
            model: Some(model.clone()),
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::BatchPending {
            head_sha,
            batch_id,
            model,
            ..
        } => ExtractedStateInfo {
            state_name: "BatchPending".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: Some(batch_id.0.clone()),
            model: Some(model.clone()),
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::Completed { head_sha, .. } => ExtractedStateInfo {
            state_name: "Completed".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: None,
            model: None,
            failure_reason: None,
            cancellation_reason: None,
        },
        ReviewMachineState::Failed {
            head_sha, reason, ..
        } => ExtractedStateInfo {
            state_name: "Failed".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: None,
            model: None,
            failure_reason: Some(reason.to_string()),
            cancellation_reason: None,
        },
        ReviewMachineState::Cancelled {
            head_sha, reason, ..
        } => ExtractedStateInfo {
            state_name: "Cancelled".to_string(),
            head_sha: Some(head_sha.short().to_string()),
            batch_id: None,
            model: None,
            failure_reason: None,
            cancellation_reason: Some(reason.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::state::{
        BatchId, CancellationReason, CommitSha, FailureReason, ReviewResult,
    };

    fn make_pr_id(owner: &str, repo: &str, number: u64) -> StateMachinePrId {
        StateMachinePrId::new(owner, repo, number)
    }

    fn make_stored(state: ReviewMachineState) -> StoredState {
        StoredState {
            state,
            installation_id: Some(12345),
        }
    }

    #[test]
    fn test_status_data_empty() {
        let data = StatusData::from_states(vec![], "1.0.0".to_string());
        assert_eq!(data.summary.total_prs, 0);
        assert!(data.prs.is_empty());
    }

    #[test]
    fn test_status_data_counts_states() {
        let states = vec![
            (
                make_pr_id("owner1", "repo1", 1),
                make_stored(ReviewMachineState::Idle {
                    reviews_enabled: true,
                }),
            ),
            (
                make_pr_id("owner1", "repo1", 2),
                make_stored(ReviewMachineState::Idle {
                    reviews_enabled: false,
                }),
            ),
            (
                make_pr_id("owner1", "repo1", 3),
                make_stored(ReviewMachineState::BatchPending {
                    reviews_enabled: true,
                    batch_id: BatchId("batch_123".to_string()),
                    head_sha: CommitSha("abc1234567890".to_string()),
                    base_sha: CommitSha("def1234567890".to_string()),
                    comment_id: None,
                    check_run_id: None,
                    model: "gpt-4".to_string(),
                    reasoning_effort: "high".to_string(),
                }),
            ),
            (
                make_pr_id("owner1", "repo1", 4),
                make_stored(ReviewMachineState::Completed {
                    reviews_enabled: true,
                    head_sha: CommitSha("abc1234567890".to_string()),
                    result: ReviewResult {
                        reasoning: "test".to_string(),
                        substantive_comments: false,
                        summary: "test".to_string(),
                    },
                }),
            ),
            (
                make_pr_id("owner1", "repo1", 5),
                make_stored(ReviewMachineState::Failed {
                    reviews_enabled: true,
                    head_sha: CommitSha("abc1234567890".to_string()),
                    reason: FailureReason::BatchExpired,
                }),
            ),
            (
                make_pr_id("owner1", "repo1", 6),
                make_stored(ReviewMachineState::Cancelled {
                    reviews_enabled: true,
                    head_sha: CommitSha("abc1234567890".to_string()),
                    reason: CancellationReason::UserRequested,
                    pending_cancel_batch_id: None,
                }),
            ),
        ];

        let data = StatusData::from_states(states, "1.0.0".to_string());

        assert_eq!(data.summary.total_prs, 6);
        assert_eq!(data.summary.idle, 2);
        assert_eq!(data.summary.batch_pending, 1);
        assert_eq!(data.summary.completed, 1);
        assert_eq!(data.summary.failed, 1);
        assert_eq!(data.summary.cancelled, 1);
        assert_eq!(data.prs.len(), 6);
    }

    #[test]
    fn test_pr_entry_extracts_info() {
        let state = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId("batch_abc123".to_string()),
            head_sha: CommitSha("1234567890abcdef".to_string()),
            base_sha: CommitSha("fedcba0987654321".to_string()),
            comment_id: None,
            check_run_id: None,
            model: "gpt-4o".to_string(),
            reasoning_effort: "high".to_string(),
        };

        let data = StatusData::from_states(
            vec![(make_pr_id("owner", "repo", 42), make_stored(state))],
            "1.0.0".to_string(),
        );

        assert_eq!(data.prs.len(), 1);
        let entry = &data.prs[0];
        assert_eq!(entry.repo_owner, "owner");
        assert_eq!(entry.repo_name, "repo");
        assert_eq!(entry.pr_number, 42);
        assert_eq!(entry.state_name, "BatchPending");
        assert_eq!(entry.head_sha, Some("1234567".to_string())); // Truncated to 7 chars
        assert_eq!(entry.batch_id, Some("batch_abc123".to_string()));
        assert_eq!(entry.model, Some("gpt-4o".to_string()));
        assert!(entry.reviews_enabled);
    }

    #[test]
    fn test_failure_reason_included() {
        let state = ReviewMachineState::Failed {
            reviews_enabled: true,
            head_sha: CommitSha("1234567890abcdef".to_string()),
            reason: FailureReason::BatchFailed {
                error: Some("rate limited".to_string()),
            },
        };

        let data = StatusData::from_states(
            vec![(make_pr_id("owner", "repo", 1), make_stored(state))],
            "1.0.0".to_string(),
        );

        let entry = &data.prs[0];
        assert_eq!(entry.state_name, "Failed");
        assert!(entry
            .failure_reason
            .as_ref()
            .unwrap()
            .contains("rate limited"));
    }

    #[test]
    fn test_cancellation_reason_included() {
        let state = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha("1234567890abcdef".to_string()),
            reason: CancellationReason::DiffTooLarge,
            pending_cancel_batch_id: None,
        };

        let data = StatusData::from_states(
            vec![(make_pr_id("owner", "repo", 1), make_stored(state))],
            "1.0.0".to_string(),
        );

        let entry = &data.prs[0];
        assert_eq!(entry.state_name, "Cancelled");
        assert!(entry
            .cancellation_reason
            .as_ref()
            .unwrap()
            .contains("diff too large"));
    }
}
