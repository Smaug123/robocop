pub mod batch_processor;
pub mod command;
pub mod config;
pub mod db;
pub mod git;
pub mod github;
pub mod openai;
pub mod persistent_store;
pub mod review_state;
pub mod state_machine;
pub mod webhook;

use std::sync::Arc;

pub use github::*;
pub use openai::*;
pub use persistent_store::PersistentStateStore;
pub use review_state::{PullRequestId, ReviewState};
pub use state_machine::{StateMachinePrId, StateStore};

// Re-export recording types from robocop_core
pub use robocop_core::{
    CorrelationId, Direction, EventType, RecordedEvent, RecordingLogger, RecordingMiddleware,
    Sanitizer, ServiceType, CORRELATION_ID_HEADER,
};

/// The name used for GitHub check runs created by robocop.
/// This appears in the Checks tab of pull requests.
pub const CHECK_RUN_NAME: &str = "Robocop Code Review";

/// Returns the bot version (delegates to robocop_core::get_library_version).
pub fn get_bot_version() -> String {
    robocop_core::get_library_version()
}

pub struct AppState {
    pub github_client: Arc<GitHubClient>,
    pub openai_client: Arc<OpenAIClient>,
    pub webhook_secret: String,
    pub target_user_id: u64,
    pub state_store: Arc<PersistentStateStore>,
    pub recording_logger: Option<RecordingLogger>,
}
