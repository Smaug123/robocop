pub mod batch_processor;
pub mod command;
pub mod config;
pub mod git;
pub mod github;
pub mod openai;
pub mod review_state;
pub mod webhook;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use github::*;
pub use openai::*;
pub use review_state::{PullRequestId, ReviewState};

// Re-export recording types from robocop_core
pub use robocop_core::{
    CorrelationId, Direction, EventType, RecordedEvent, RecordingLogger, RecordingMiddleware,
    Sanitizer, ServiceType, CORRELATION_ID_HEADER,
};

/// The context string used for GitHub commit statuses posted by robocop.
/// This is used with GitHub branch protection rules to require reviews.
pub const COMMIT_STATUS_CONTEXT: &str = "robocop/code-review";

/// Returns the bot version (delegates to robocop_core::get_library_version).
pub fn get_bot_version() -> String {
    robocop_core::get_library_version()
}

#[derive(Debug, Clone)]
pub struct PendingBatch {
    pub batch_id: String,
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub comment_id: u64,
    pub version: String,
    pub created_at: u64,
    pub head_sha: String,
    pub base_sha: String,
}

pub struct AppState {
    pub github_client: GitHubClient,
    pub openai_client: OpenAIClient,
    pub webhook_secret: String,
    pub target_user_id: u64,
    pub pending_batches: Arc<RwLock<HashMap<String, PendingBatch>>>,
    pub review_states: Arc<RwLock<HashMap<PullRequestId, ReviewState>>>,
    pub recording_logger: Option<RecordingLogger>,
}
