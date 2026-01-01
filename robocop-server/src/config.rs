use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub github_app_id: u64,
    pub github_private_key: String,
    pub github_webhook_secret: String,
    pub openai_api_key: String,
    pub target_user_id: u64,
    pub port: u16,
    pub recording_enabled: bool,
    pub recording_log_path: String,
    /// Directory for persistent state (SQLite database).
    /// Defaults to current working directory.
    pub state_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let github_app_id = env::var("GITHUB_APP_ID")
            .context("GITHUB_APP_ID environment variable is required")?
            .parse::<u64>()
            .context("GITHUB_APP_ID must be a valid number")?;

        let github_private_key = env::var("GITHUB_PRIVATE_KEY")
            .context("GITHUB_PRIVATE_KEY environment variable is required")?
            .replace("\\n", "\n");

        let github_webhook_secret = env::var("GITHUB_WEBHOOK_SECRET")
            .context("GITHUB_WEBHOOK_SECRET environment variable is required")?;

        let openai_api_key = env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY environment variable is required")?;

        let target_user_id = env::var("TARGET_GITHUB_USER_ID")
            .context("TARGET_GITHUB_USER_ID environment variable is required")?
            .parse::<u64>()
            .context("TARGET_GITHUB_USER_ID must be a valid number")?;

        let port = env::var("PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse::<u16>()
            .context("PORT must be a valid number")?;

        let recording_enabled = env::var("RECORDING_ENABLED")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);

        let recording_log_path =
            env::var("RECORDING_LOG_PATH").unwrap_or_else(|_| "recordings.jsonl".to_string());

        let state_dir = env::var("STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));

        Ok(Config {
            github_app_id,
            github_private_key,
            github_webhook_secret,
            openai_api_key,
            target_user_id,
            port,
            recording_enabled,
            recording_log_path,
            state_dir,
        })
    }
}
