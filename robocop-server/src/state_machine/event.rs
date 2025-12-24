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
    },

    /// Failed to fetch data (empty diff, too large, fetch error, etc.).
    DataFetchFailed { reason: DataFetchFailure },

    // =========================================================================
    // Batch Submission Results
    // =========================================================================
    /// Batch was successfully submitted to OpenAI.
    BatchSubmitted {
        batch_id: BatchId,
        comment_id: CommentId,
        check_run_id: CheckRunId,
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

    #[test]
    fn test_event_construction() {
        let event = Event::PrUpdated {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
            force_review: false,
            options: ReviewOptions::default(),
        };

        if let Event::PrUpdated {
            head_sha,
            force_review,
            ..
        } = event
        {
            assert_eq!(head_sha.0, "abc123");
            assert!(!force_review);
        } else {
            panic!("Expected PrUpdated event");
        }
    }
}
