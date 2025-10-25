pub mod batch_processor;
pub mod command;
pub mod config;
pub mod git;
pub mod github;
pub mod openai;
pub mod recording;
pub mod webhook;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use github::*;
pub use openai::*;
pub use recording::RecordingLogger;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub fn get_bot_version() -> String {
    if let Some(git_hash) = built_info::GIT_COMMIT_HASH {
        if git_hash.len() >= 8 {
            git_hash[..8].to_string()
        } else {
            git_hash.to_string()
        }
    } else {
        "unknown".to_string()
    }
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
    pub recording_logger: Option<RecordingLogger>,
}
