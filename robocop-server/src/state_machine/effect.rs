//! Effects (side effects as data).
//!
//! Effects describe what should happen as a result of a state transition.
//! They are pure data - the interpreter executes them against real APIs.
//! This separation enables testing the transition logic without mocking HTTP.

use super::state::{
    BatchId, CancellationReason, CheckRunId, CommitSha, FailureReason, ReviewOptions, ReviewResult,
};

/// All effects that can be produced by state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    // =========================================================================
    // GitHub Effects
    // =========================================================================
    /// Fetch diff and file contents from GitHub.
    FetchData {
        head_sha: CommitSha,
        base_sha: CommitSha,
    },

    /// Check if old_sha is an ancestor of new_sha.
    CheckAncestry {
        old_sha: CommitSha,
        new_sha: CommitSha,
    },

    /// Create or update the robocop comment on the PR.
    UpdateComment { content: CommentContent },

    /// Create a new check run.
    CreateCheckRun {
        head_sha: CommitSha,
        status: EffectCheckRunStatus,
        /// Required when status is Completed.
        conclusion: Option<EffectCheckRunConclusion>,
        title: String,
        summary: String,
    },

    /// Update an existing check run.
    UpdateCheckRun {
        check_run_id: CheckRunId,
        status: EffectCheckRunStatus,
        conclusion: Option<EffectCheckRunConclusion>,
        title: String,
        summary: String,
        /// Optional external ID to associate with the check run (e.g., batch ID).
        external_id: Option<BatchId>,
    },

    // =========================================================================
    // OpenAI Effects
    // =========================================================================
    /// Submit a batch to OpenAI for processing.
    SubmitBatch {
        diff: String,
        file_contents: Vec<(String, String)>,
        head_sha: CommitSha,
        base_sha: CommitSha,
        options: ReviewOptions,
    },

    /// Cancel a pending batch.
    CancelBatch { batch_id: BatchId },

    /// Download and parse batch output.
    DownloadBatchOutput {
        batch_id: BatchId,
        output_file_id: String,
    },

    // =========================================================================
    // Logging Effects
    // =========================================================================
    /// Log a message (for debugging/tracing).
    Log { level: LogLevel, message: String },
}

/// Content for the robocop comment on a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentContent {
    /// Review is in progress.
    InProgress {
        head_sha: CommitSha,
        batch_id: BatchId,
        model: String,
        reasoning_effort: String,
    },

    /// Review completed successfully.
    ReviewComplete {
        head_sha: CommitSha,
        batch_id: BatchId,
        result: ReviewResult,
    },

    /// Review failed.
    ReviewFailed {
        head_sha: CommitSha,
        batch_id: BatchId,
        reason: FailureReason,
    },

    /// Review was cancelled.
    ReviewCancelled {
        head_sha: CommitSha,
        reason: CancellationReason,
    },

    /// Review was suppressed (reviews disabled).
    ReviewSuppressed { head_sha: CommitSha },

    /// Diff was too large to review.
    DiffTooLarge {
        head_sha: CommitSha,
        skipped_files: Vec<String>,
        total_files: usize,
    },

    /// Reviews were enabled.
    ReviewsEnabled { head_sha: CommitSha },

    /// Reviews were disabled.
    ReviewsDisabled { cancelled_count: usize },

    /// No reviews to cancel.
    NoReviewsToCancel,

    /// Unrecognized command.
    UnrecognizedCommand { attempted: String },
}

/// Check run status for effects.
/// Mirrors the GitHub API but is our own type for independence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCheckRunStatus {
    Queued,
    InProgress,
    Completed,
}

impl EffectCheckRunStatus {
    /// Convert to the GitHub API type.
    pub fn to_github(&self) -> crate::github::CheckRunStatus {
        match self {
            Self::Queued => crate::github::CheckRunStatus::Queued,
            Self::InProgress => crate::github::CheckRunStatus::InProgress,
            Self::Completed => crate::github::CheckRunStatus::Completed,
        }
    }
}

/// Check run conclusion for effects.
/// Mirrors the GitHub API but is our own type for independence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCheckRunConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    Stale,
    TimedOut,
}

impl EffectCheckRunConclusion {
    /// Convert to the GitHub API type.
    pub fn to_github(&self) -> crate::github::CheckRunConclusion {
        match self {
            Self::Success => crate::github::CheckRunConclusion::Success,
            Self::Failure => crate::github::CheckRunConclusion::Failure,
            Self::Cancelled => crate::github::CheckRunConclusion::Cancelled,
            Self::Skipped => crate::github::CheckRunConclusion::Skipped,
            Self::Stale => crate::github::CheckRunConclusion::Stale,
            Self::TimedOut => crate::github::CheckRunConclusion::TimedOut,
        }
    }
}

/// Log level for logging effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_construction() {
        let effect = Effect::FetchData {
            head_sha: CommitSha::from("abc123"),
            base_sha: CommitSha::from("def456"),
        };

        if let Effect::FetchData { head_sha, base_sha } = effect {
            assert_eq!(head_sha.0, "abc123");
            assert_eq!(base_sha.0, "def456");
        } else {
            panic!("Expected FetchData effect");
        }
    }

    #[test]
    fn test_comment_content_variants() {
        let content = CommentContent::InProgress {
            head_sha: CommitSha::from("abc123"),
            batch_id: BatchId::from("batch_123".to_string()),
            model: "gpt-4".to_string(),
            reasoning_effort: "high".to_string(),
        };

        if let CommentContent::InProgress {
            head_sha, model, ..
        } = content
        {
            assert_eq!(head_sha.0, "abc123");
            assert_eq!(model, "gpt-4");
        } else {
            panic!("Expected InProgress content");
        }
    }

    #[test]
    fn test_check_run_status_conversion() {
        let status = EffectCheckRunStatus::InProgress;
        let github_status = status.to_github();
        assert_eq!(github_status.as_str(), "in_progress");
    }

    #[test]
    fn test_check_run_conclusion_conversion() {
        let conclusion = EffectCheckRunConclusion::Success;
        let github_conclusion = conclusion.to_github();
        assert_eq!(github_conclusion.as_str(), "success");

        let conclusion = EffectCheckRunConclusion::Stale;
        let github_conclusion = conclusion.to_github();
        assert_eq!(github_conclusion.as_str(), "stale");
    }
}
