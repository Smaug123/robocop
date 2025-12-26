//! State types for the review state machine.
//!
//! This module defines the explicit state machine for a single PR's review lifecycle.
//! Following the principle of "make illegal states unrepresentable", we use
//! an enum that captures exactly what states are valid.

use serde::Deserialize;
use std::fmt;

/// Newtype for commit SHA to prevent mixing with other strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitSha(pub String);

impl CommitSha {
    /// Returns a truncated SHA for display (first 7 characters).
    pub fn short(&self) -> &str {
        &self.0[..7.min(self.0.len())]
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for CommitSha {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for CommitSha {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Newtype for OpenAI batch ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BatchId(pub String);

impl fmt::Display for BatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for BatchId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Newtype for GitHub comment ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommentId(pub u64);

impl From<u64> for CommentId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

/// Newtype for GitHub check run ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CheckRunId(pub u64);

impl From<u64> for CheckRunId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

/// Result of a completed code review.
/// Matches the schema sent to OpenAI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewResult {
    pub reasoning: String,
    #[serde(rename = "substantiveComments")]
    pub substantive_comments: bool,
    pub summary: String,
}

/// Reason why a batch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureReason {
    /// OpenAI batch processing failed.
    BatchFailed { error: Option<String> },
    /// Batch expired before completion.
    BatchExpired,
    /// Batch was cancelled (externally or by us).
    BatchCancelled,
    /// Failed to download output file.
    DownloadFailed { error: String },
    /// Failed to parse review output.
    ParseFailed { error: String },
    /// Batch completed but had no output file.
    NoOutputFile,
    /// Batch submission failed.
    SubmissionFailed { error: String },
    /// Data fetch failed (diff too large, etc.).
    DataFetchFailed { reason: String },
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BatchFailed { error } => match error {
                Some(e) => write!(f, "batch processing failed: {}", e),
                None => write!(f, "batch processing failed"),
            },
            Self::BatchExpired => write!(f, "batch expired"),
            Self::BatchCancelled => write!(f, "batch was cancelled"),
            Self::DownloadFailed { error } => write!(f, "download failed: {}", error),
            Self::ParseFailed { error } => write!(f, "parse failed: {}", error),
            Self::NoOutputFile => write!(f, "no output file"),
            Self::SubmissionFailed { error } => write!(f, "submission failed: {}", error),
            Self::DataFetchFailed { reason } => write!(f, "data fetch failed: {}", reason),
        }
    }
}

/// Reason why a review was cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationReason {
    /// User explicitly requested cancellation via command.
    UserRequested,
    /// A newer commit superseded this review.
    Superseded { new_sha: CommitSha },
    /// Reviews were disabled for this PR.
    ReviewsDisabled,
    /// Batch was cancelled externally (e.g., via OpenAI dashboard or API).
    External,
    /// No changes to review (empty diff).
    NoChanges,
    /// Diff was too large to review.
    DiffTooLarge,
    /// No files to review after filtering.
    NoFiles,
}

impl fmt::Display for CancellationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserRequested => write!(f, "cancelled by user"),
            Self::Superseded { new_sha } => write!(f, "superseded by {}", new_sha.short()),
            Self::ReviewsDisabled => write!(f, "reviews disabled"),
            Self::External => write!(f, "cancelled externally"),
            Self::NoChanges => write!(f, "no changes to review"),
            Self::DiffTooLarge => write!(f, "diff too large"),
            Self::NoFiles => write!(f, "no files to review"),
        }
    }
}

/// Options for a review request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewOptions {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl From<crate::command::ReviewOptions> for ReviewOptions {
    fn from(opts: crate::command::ReviewOptions) -> Self {
        Self {
            model: opts.model,
            reasoning_effort: opts.reasoning_effort,
        }
    }
}

/// The explicit state machine for a single PR's review lifecycle.
///
/// Each variant represents a distinct state the review can be in.
/// The `reviews_enabled` field tracks whether automatic reviews are on/off for this PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMachineState {
    /// No active review for this PR/commit.
    Idle { reviews_enabled: bool },

    /// Fetching diff and file contents before submitting batch.
    Preparing {
        reviews_enabled: bool,
        head_sha: CommitSha,
        base_sha: CommitSha,
        options: ReviewOptions,
    },

    /// Waiting for ancestry check result before deciding what to do.
    AwaitingAncestryCheck {
        reviews_enabled: bool,
        /// The batch currently pending.
        batch_id: BatchId,
        head_sha: CommitSha,
        base_sha: CommitSha,
        /// Comment ID if one was created (may be None if GitHub API failed).
        comment_id: Option<CommentId>,
        /// Check run ID if one was created (may be None if GitHub API failed).
        check_run_id: Option<CheckRunId>,
        model: String,
        reasoning_effort: String,
        /// The new commit that triggered the ancestry check.
        new_head_sha: CommitSha,
        new_base_sha: CommitSha,
        /// Options from the new PR update event (to use if superseded).
        new_options: ReviewOptions,
    },

    /// Batch submitted to OpenAI, awaiting completion.
    BatchPending {
        reviews_enabled: bool,
        batch_id: BatchId,
        head_sha: CommitSha,
        base_sha: CommitSha,
        /// Comment ID if one was created (may be None if GitHub API failed).
        comment_id: Option<CommentId>,
        /// Check run ID if one was created (may be None if GitHub API failed).
        check_run_id: Option<CheckRunId>,
        model: String,
        reasoning_effort: String,
    },

    /// Review completed successfully (terminal state for this commit).
    Completed {
        reviews_enabled: bool,
        head_sha: CommitSha,
        result: ReviewResult,
    },

    /// Review failed (terminal state for this commit).
    Failed {
        reviews_enabled: bool,
        head_sha: CommitSha,
        reason: FailureReason,
    },

    /// Review was cancelled (terminal state for this commit).
    Cancelled {
        reviews_enabled: bool,
        head_sha: CommitSha,
        reason: CancellationReason,
        /// Batch ID still being cancelled (for polling in case cancel fails).
        /// When a batch is cancelled, the cancel API call might fail. If it does,
        /// we keep tracking the batch here so polling can still pick up the result
        /// if the batch completes before we successfully cancel it.
        pending_cancel_batch_id: Option<BatchId>,
    },
}

impl ReviewMachineState {
    /// Returns whether reviews are enabled for this PR.
    pub fn reviews_enabled(&self) -> bool {
        match self {
            Self::Idle { reviews_enabled } => *reviews_enabled,
            Self::Preparing {
                reviews_enabled, ..
            } => *reviews_enabled,
            Self::AwaitingAncestryCheck {
                reviews_enabled, ..
            } => *reviews_enabled,
            Self::BatchPending {
                reviews_enabled, ..
            } => *reviews_enabled,
            Self::Completed {
                reviews_enabled, ..
            } => *reviews_enabled,
            Self::Failed {
                reviews_enabled, ..
            } => *reviews_enabled,
            Self::Cancelled {
                reviews_enabled, ..
            } => *reviews_enabled,
        }
    }

    /// Returns the head SHA if the state has one.
    pub fn head_sha(&self) -> Option<&CommitSha> {
        match self {
            Self::Idle { .. } => None,
            Self::Preparing { head_sha, .. } => Some(head_sha),
            Self::AwaitingAncestryCheck { head_sha, .. } => Some(head_sha),
            Self::BatchPending { head_sha, .. } => Some(head_sha),
            Self::Completed { head_sha, .. } => Some(head_sha),
            Self::Failed { head_sha, .. } => Some(head_sha),
            Self::Cancelled { head_sha, .. } => Some(head_sha),
        }
    }

    /// Returns the batch ID if there's a pending batch.
    ///
    /// This includes batches in active states (BatchPending, AwaitingAncestryCheck)
    /// as well as batches in Cancelled state where cancel might have failed.
    pub fn pending_batch_id(&self) -> Option<&BatchId> {
        match self {
            Self::BatchPending { batch_id, .. } => Some(batch_id),
            Self::AwaitingAncestryCheck { batch_id, .. } => Some(batch_id),
            Self::Cancelled {
                pending_cancel_batch_id: Some(batch_id),
                ..
            } => Some(batch_id),
            _ => None,
        }
    }

    /// Returns the base SHA if the state has one.
    pub fn base_sha(&self) -> Option<&CommitSha> {
        match self {
            Self::Idle { .. } => None,
            Self::Preparing { base_sha, .. } => Some(base_sha),
            Self::AwaitingAncestryCheck { base_sha, .. } => Some(base_sha),
            Self::BatchPending { base_sha, .. } => Some(base_sha),
            Self::Completed { .. } => None,
            Self::Failed { .. } => None,
            Self::Cancelled { .. } => None,
        }
    }

    /// Returns the new head SHA for ancestry checking (only in AwaitingAncestryCheck state).
    pub fn ancestry_new_head_sha(&self) -> Option<&CommitSha> {
        match self {
            Self::AwaitingAncestryCheck { new_head_sha, .. } => Some(new_head_sha),
            _ => None,
        }
    }

    /// Returns the check run ID if the state has one.
    pub fn check_run_id(&self) -> Option<CheckRunId> {
        match self {
            Self::BatchPending { check_run_id, .. } => *check_run_id,
            Self::AwaitingAncestryCheck { check_run_id, .. } => *check_run_id,
            _ => None,
        }
    }

    /// Returns true if this is a terminal state (Completed, Failed, or Cancelled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }

    /// Returns true if this state has an active (pending) batch.
    pub fn has_pending_batch(&self) -> bool {
        matches!(
            self,
            Self::BatchPending { .. } | Self::AwaitingAncestryCheck { .. }
        )
    }

    /// Creates a new Idle state with the specified reviews_enabled flag.
    pub fn idle(reviews_enabled: bool) -> Self {
        Self::Idle { reviews_enabled }
    }

    /// Returns a new state with the reviews_enabled flag updated.
    ///
    /// This is used when the PR description is edited to add/remove the
    /// "no review" marker, and we need to update the existing state.
    pub fn with_reviews_enabled(self, enabled: bool) -> Self {
        match self {
            Self::Idle { .. } => Self::Idle {
                reviews_enabled: enabled,
            },
            Self::Preparing {
                head_sha,
                base_sha,
                options,
                ..
            } => Self::Preparing {
                reviews_enabled: enabled,
                head_sha,
                base_sha,
                options,
            },
            Self::AwaitingAncestryCheck {
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
            } => Self::AwaitingAncestryCheck {
                reviews_enabled: enabled,
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
            },
            Self::BatchPending {
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
                ..
            } => Self::BatchPending {
                reviews_enabled: enabled,
                batch_id,
                head_sha,
                base_sha,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            },
            Self::Completed {
                head_sha, result, ..
            } => Self::Completed {
                reviews_enabled: enabled,
                head_sha,
                result,
            },
            Self::Failed {
                head_sha, reason, ..
            } => Self::Failed {
                reviews_enabled: enabled,
                head_sha,
                reason,
            },
            Self::Cancelled {
                head_sha,
                reason,
                pending_cancel_batch_id,
                ..
            } => Self::Cancelled {
                reviews_enabled: enabled,
                head_sha,
                reason,
                pending_cancel_batch_id,
            },
        }
    }
}

impl Default for ReviewMachineState {
    fn default() -> Self {
        Self::Idle {
            reviews_enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_sha_short() {
        let sha = CommitSha("abc123def456".to_string());
        assert_eq!(sha.short(), "abc123d");

        let short_sha = CommitSha("abc".to_string());
        assert_eq!(short_sha.short(), "abc");
    }

    #[test]
    fn test_state_reviews_enabled() {
        let idle_enabled = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        assert!(idle_enabled.reviews_enabled());

        let idle_disabled = ReviewMachineState::Idle {
            reviews_enabled: false,
        };
        assert!(!idle_disabled.reviews_enabled());
    }

    #[test]
    fn test_state_is_terminal() {
        let idle = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        assert!(!idle.is_terminal());

        let completed = ReviewMachineState::Completed {
            reviews_enabled: true,
            head_sha: CommitSha("abc123".into()),
            result: ReviewResult {
                reasoning: "Code looks good".into(),
                substantive_comments: false,
                summary: "LGTM".into(),
            },
        };
        assert!(completed.is_terminal());

        let failed = ReviewMachineState::Failed {
            reviews_enabled: true,
            head_sha: CommitSha("abc123".into()),
            reason: FailureReason::BatchExpired,
        };
        assert!(failed.is_terminal());

        let cancelled = ReviewMachineState::Cancelled {
            reviews_enabled: true,
            head_sha: CommitSha("abc123".into()),
            reason: CancellationReason::UserRequested,
            pending_cancel_batch_id: None,
        };
        assert!(cancelled.is_terminal());
    }

    #[test]
    fn test_state_has_pending_batch() {
        let idle = ReviewMachineState::Idle {
            reviews_enabled: true,
        };
        assert!(!idle.has_pending_batch());

        let pending = ReviewMachineState::BatchPending {
            reviews_enabled: true,
            batch_id: BatchId("batch_123".into()),
            head_sha: CommitSha("abc123".into()),
            base_sha: CommitSha("def456".into()),
            comment_id: Some(CommentId(1)),
            check_run_id: Some(CheckRunId(2)),
            model: "gpt-4".into(),
            reasoning_effort: "high".into(),
        };
        assert!(pending.has_pending_batch());
    }

    #[test]
    fn test_failure_reason_display() {
        let reason = FailureReason::BatchExpired;
        assert_eq!(format!("{}", reason), "batch expired");

        let reason = FailureReason::BatchFailed {
            error: Some("timeout".into()),
        };
        assert_eq!(format!("{}", reason), "batch processing failed: timeout");
    }

    #[test]
    fn test_cancellation_reason_display() {
        let reason = CancellationReason::UserRequested;
        assert_eq!(format!("{}", reason), "cancelled by user");

        let reason = CancellationReason::Superseded {
            new_sha: CommitSha("abc123def".into()),
        };
        assert_eq!(format!("{}", reason), "superseded by abc123d");
    }
}
