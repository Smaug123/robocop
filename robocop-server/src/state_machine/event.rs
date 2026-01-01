//! Events that trigger state transitions.
//!
//! Events represent things that happened - webhooks received, API responses,
//! polling results, etc. They are inputs to the pure transition function.

use super::state::{
    BatchId, CheckRunId, CommentId, CommitSha, FailureReason, ReviewOptions, ReviewResult,
};

/// All events that can trigger state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    // =========================================================================
    // Webhook Events
    // =========================================================================
    /// PR was opened, synchronized (new commit pushed), or edited.
    /// Triggered by pull_request.opened, pull_request.synchronize, pull_request.edited webhooks.
    PrUpdated {
        head_sha: CommitSha,
        base_sha: CommitSha,
        /// If true, this is a forced review (from @robocop review command).
        force_review: bool,
        /// Review options extracted from PR description or command.
        options: ReviewOptions,
    },

    /// User requested a manual review via @robocop review command.
    ReviewRequested {
        head_sha: CommitSha,
        base_sha: CommitSha,
        options: ReviewOptions,
    },

    /// User requested to cancel all pending reviews via @robocop cancel command.
    CancelRequested,

    /// User requested to enable automatic reviews via @robocop enable-reviews command.
    EnableReviewsRequested {
        head_sha: CommitSha,
        base_sha: CommitSha,
        /// Review options extracted from PR description (model, reasoning_effort).
        options: ReviewOptions,
    },

    /// User requested to disable automatic reviews via @robocop disable-reviews command.
    DisableReviewsRequested,

    // =========================================================================
    // Data Fetch Results
    // =========================================================================
    /// Diff and file contents were successfully fetched.
    DataFetched {
        diff: String,
        file_contents: Vec<FileContent>,
        /// Deterministic reconciliation token for crash recovery.
        /// Generated from PR identity (installation_id, repo, pr_number) + head_sha.
        /// Using a deterministic token ensures retries for the same PR+commit
        /// will look for the same batch, preventing duplicate batch submissions.
        reconciliation_token: String,
    },

    /// Failed to fetch data (empty diff, too large, fetch error, etc.).
    DataFetchFailed { reason: DataFetchFailure },

    // =========================================================================
    // Batch Submission Results
    // =========================================================================
    /// Batch was successfully submitted to OpenAI.
    BatchSubmitted {
        batch_id: BatchId,
        /// Comment ID if one was created (may be None if GitHub API failed).
        comment_id: Option<CommentId>,
        /// Check run ID if one was created (may be None if GitHub API failed).
        check_run_id: Option<CheckRunId>,
        model: String,
        reasoning_effort: String,
    },

    /// Batch submission failed.
    BatchSubmissionFailed {
        error: String,
        /// Comment ID if a comment was created before failure.
        comment_id: Option<CommentId>,
        /// Check run ID if a check run was created before failure.
        check_run_id: Option<CheckRunId>,
    },

    // =========================================================================
    // Polling Results
    // =========================================================================
    /// Batch status update from polling (still processing).
    BatchStatusUpdate {
        batch_id: BatchId,
        status: BatchStatus,
    },

    /// Batch completed successfully with results.
    BatchCompleted {
        batch_id: BatchId,
        result: ReviewResult,
    },

    /// Batch terminated without success (failed, expired, cancelled).
    BatchTerminated {
        batch_id: BatchId,
        reason: FailureReason,
    },

    // =========================================================================
    // Ancestry Check Results
    // =========================================================================
    /// Result of checking whether old commit is ancestor of new commit.
    AncestryResult {
        old_sha: CommitSha,
        new_sha: CommitSha,
        /// True if old_sha is an ancestor of new_sha (old is superseded).
        is_superseded: bool,
    },

    /// Ancestry check failed (e.g., GitHub API error).
    /// This is separate from AncestryResult to allow explicit error handling -
    /// we don't want transient errors to cancel valid in-flight batches.
    AncestryCheckFailed {
        old_sha: CommitSha,
        new_sha: CommitSha,
        error: String,
    },

    // =========================================================================
    // Reconciliation Events
    // =========================================================================
    /// Startup reconciliation found a batch at OpenAI matching a BatchSubmitting state.
    ///
    /// This event is emitted during startup recovery when we find a batch at OpenAI
    /// that matches the reconciliation_token from a persisted BatchSubmitting state.
    ReconciliationComplete {
        /// The batch ID found at OpenAI.
        batch_id: BatchId,
        /// Comment ID if one was created before the crash.
        comment_id: Option<CommentId>,
        /// Check run ID if one was created before the crash.
        check_run_id: Option<CheckRunId>,
        /// The model used for this review.
        model: String,
        /// The reasoning effort used for this review.
        reasoning_effort: String,
    },

    /// Startup reconciliation failed to find a matching batch at OpenAI.
    ///
    /// This means the batch was never submitted (crash happened before OpenAI call),
    /// or the batch expired/was cancelled.
    ReconciliationFailed {
        /// The reconciliation token that we were looking for.
        reconciliation_token: String,
        /// Why reconciliation failed.
        error: String,
    },
}

impl Event {
    /// Returns a summary of the event suitable for logging.
    ///
    /// This avoids logging potentially large/sensitive data like diffs and file contents.
    pub fn log_summary(&self) -> String {
        match self {
            Event::PrUpdated {
                head_sha,
                base_sha,
                force_review,
                ..
            } => {
                format!(
                    "PrUpdated {{ head: {}, base: {}, force: {} }}",
                    head_sha.short(),
                    base_sha.short(),
                    force_review
                )
            }
            Event::ReviewRequested {
                head_sha, base_sha, ..
            } => {
                format!(
                    "ReviewRequested {{ head: {}, base: {} }}",
                    head_sha.short(),
                    base_sha.short()
                )
            }
            Event::CancelRequested => "CancelRequested".to_string(),
            Event::EnableReviewsRequested {
                head_sha, base_sha, ..
            } => {
                format!(
                    "EnableReviewsRequested {{ head: {}, base: {} }}",
                    head_sha.short(),
                    base_sha.short()
                )
            }
            Event::DisableReviewsRequested => "DisableReviewsRequested".to_string(),
            Event::DataFetched {
                diff,
                file_contents,
                reconciliation_token,
            } => {
                format!(
                    "DataFetched {{ diff_len: {}, file_count: {}, token: {}... }}",
                    diff.len(),
                    file_contents.len(),
                    &reconciliation_token[..8.min(reconciliation_token.len())]
                )
            }
            Event::DataFetchFailed { reason } => {
                format!("DataFetchFailed {{ reason: {:?} }}", reason)
            }
            Event::BatchSubmitted {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            } => {
                format!(
                    "BatchSubmitted {{ batch: {}, comment: {:?}, check_run: {:?}, model: {}, reasoning: {} }}",
                    batch_id, comment_id.map(|c| c.0), check_run_id.map(|c| c.0), model, reasoning_effort
                )
            }
            Event::BatchSubmissionFailed { error, .. } => {
                format!("BatchSubmissionFailed {{ error: {} }}", error)
            }
            Event::BatchStatusUpdate { batch_id, status } => {
                format!(
                    "BatchStatusUpdate {{ batch: {}, status: {:?} }}",
                    batch_id, status
                )
            }
            Event::BatchCompleted { batch_id, result } => {
                let result_summary = if result.substantive_comments {
                    "HasIssues"
                } else {
                    "NoIssues"
                };
                format!(
                    "BatchCompleted {{ batch: {}, result: {} }}",
                    batch_id, result_summary
                )
            }
            Event::BatchTerminated { batch_id, reason } => {
                format!(
                    "BatchTerminated {{ batch: {}, reason: {:?} }}",
                    batch_id, reason
                )
            }
            Event::AncestryResult {
                old_sha,
                new_sha,
                is_superseded,
            } => {
                format!(
                    "AncestryResult {{ old: {}, new: {}, superseded: {} }}",
                    old_sha.short(),
                    new_sha.short(),
                    is_superseded
                )
            }
            Event::AncestryCheckFailed {
                old_sha,
                new_sha,
                error,
            } => {
                format!(
                    "AncestryCheckFailed {{ old: {}, new: {}, error: {} }}",
                    old_sha.short(),
                    new_sha.short(),
                    error
                )
            }
            Event::ReconciliationComplete {
                batch_id,
                comment_id,
                check_run_id,
                model,
                reasoning_effort,
            } => {
                format!(
                    "ReconciliationComplete {{ batch: {}, comment: {:?}, check_run: {:?}, model: {}, reasoning: {} }}",
                    batch_id, comment_id.map(|c| c.0), check_run_id.map(|c| c.0), model, reasoning_effort
                )
            }
            Event::ReconciliationFailed {
                reconciliation_token,
                error,
            } => {
                format!(
                    "ReconciliationFailed {{ token: {}, error: {} }}",
                    reconciliation_token, error
                )
            }
        }
    }
}

/// File content fetched from the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    pub path: String,
    pub content: String,
}

/// Reasons why data fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFetchFailure {
    /// Diff was empty (no changes).
    EmptyDiff,
    /// Total size exceeded limits.
    TooLarge {
        skipped_files: Vec<String>,
        total_files: usize,
    },
    /// Error fetching from GitHub API.
    FetchError { error: String },
    /// No files could be retrieved.
    NoFiles,
}

/// Status of an OpenAI batch (from polling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStatus {
    /// Batch is being validated.
    Validating,
    /// Batch is being processed.
    InProgress,
    /// Batch is finalizing.
    Finalizing,
    /// Batch completed successfully.
    Completed,
    /// Batch failed.
    Failed,
    /// Batch expired.
    Expired,
    /// Batch is being cancelled.
    Cancelling,
    /// Batch was cancelled.
    Cancelled,
}

impl BatchStatus {
    /// Returns true if this is a terminal status.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Expired | Self::Cancelled
        )
    }

    /// Returns true if this status indicates the batch is still processing.
    pub fn is_processing(&self) -> bool {
        matches!(
            self,
            Self::Validating | Self::InProgress | Self::Finalizing | Self::Cancelling
        )
    }

    /// Parse from OpenAI batch status string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "validating" => Some(Self::Validating),
            "in_progress" => Some(Self::InProgress),
            "finalizing" => Some(Self::Finalizing),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "expired" => Some(Self::Expired),
            "cancelling" => Some(Self::Cancelling),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_status_is_terminal() {
        assert!(!BatchStatus::Validating.is_terminal());
        assert!(!BatchStatus::InProgress.is_terminal());
        assert!(!BatchStatus::Finalizing.is_terminal());
        assert!(BatchStatus::Completed.is_terminal());
        assert!(BatchStatus::Failed.is_terminal());
        assert!(BatchStatus::Expired.is_terminal());
        assert!(BatchStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_batch_status_parse() {
        assert_eq!(
            BatchStatus::parse("validating"),
            Some(BatchStatus::Validating)
        );
        assert_eq!(
            BatchStatus::parse("in_progress"),
            Some(BatchStatus::InProgress)
        );
        assert_eq!(
            BatchStatus::parse("completed"),
            Some(BatchStatus::Completed)
        );
        assert_eq!(BatchStatus::parse("unknown"), None);
    }
}
