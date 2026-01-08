use anyhow::{Context, Result};
use std::env;
use std::fs;
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
    /// Optional bearer token for /status endpoint authentication.
    /// If set, requests to /status must include `Authorization: Bearer <token>`.
    /// If not set, /status endpoint is disabled (returns 403 Forbidden).
    pub status_auth_token: Option<String>,
    /// Optional signing secret for OpenAI webhook verification.
    /// If set, enables the /openai-webhook endpoint for real-time batch completions.
    /// If not set, /openai-webhook endpoint returns 503 Service Unavailable.
    /// Secret is base64-encoded and may have "whsec_" prefix.
    pub openai_webhook_secret: Option<String>,
}

/// Read a required config value.
///
/// For a key like "GITHUB_PRIVATE_KEY":
/// 1. Check if GITHUB_PRIVATE_KEY_FILE is set - if so, read from that file path
/// 2. Otherwise, check GITHUB_PRIVATE_KEY env var directly
///
/// When reading from env var, `\n` escape sequences are converted to actual newlines
/// for backward compatibility (needed for PEM keys stored as single-line env vars).
fn read_secret(key: &str) -> Result<String> {
    let file_key = format!("{}_FILE", key);

    if let Ok(path) = env::var(&file_key) {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {} from file: {}", key, path))?;
        // Trim trailing whitespace (files often have trailing newlines)
        Ok(contents.trim_end().to_string())
    } else {
        let value = env::var(key)
            .with_context(|| format!("{} or {} environment variable is required", key, file_key))?;
        // Convert \n escape sequences to actual newlines for backward compatibility
        // (needed for GITHUB_PRIVATE_KEY when stored as single-line env var)
        Ok(value.replace("\\n", "\n"))
    }
}

/// Read an optional config value.
///
/// For a key like "STATUS_AUTH_TOKEN":
/// 1. Check if STATUS_AUTH_TOKEN_FILE is set - if so, read from that file path (errors if file unreadable)
/// 2. Otherwise, check STATUS_AUTH_TOKEN env var directly
/// 3. Returns None if neither is set, or if the value is empty/whitespace-only
fn read_secret_optional(key: &str) -> Result<Option<String>> {
    let file_key = format!("{}_FILE", key);

    let value = if let Ok(path) = env::var(&file_key) {
        // _FILE is explicitly set - error if we can't read it
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {} from file: {}", key, path))?;
        Some(contents)
    } else {
        env::var(key).ok()
    };

    Ok(value.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let github_app_id = read_secret("GITHUB_APP_ID")?
            .parse::<u64>()
            .context("GITHUB_APP_ID must be a valid number")?;

        let github_private_key = read_secret("GITHUB_PRIVATE_KEY")?;

        let github_webhook_secret = read_secret("GITHUB_WEBHOOK_SECRET")?;

        let openai_api_key = read_secret("OPENAI_API_KEY")?;

        let target_user_id = read_secret("TARGET_GITHUB_USER_ID")?
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

        let status_auth_token = read_secret_optional("STATUS_AUTH_TOKEN")?;

        // OpenAI webhook secret for real-time batch completion notifications.
        // If not set, the /openai-webhook endpoint will return 503.
        // Supports OPENAI_WEBHOOK_SECRET_FILE for file-based secret deployments.
        let openai_webhook_secret = read_secret_optional("OPENAI_WEBHOOK_SECRET")?;

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
            status_auth_token,
            openai_webhook_secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_secret_from_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "secret-value").unwrap();

        env::set_var("TEST_SECRET_FILE", file.path());
        env::remove_var("TEST_SECRET");

        let result = read_secret("TEST_SECRET").unwrap();
        assert_eq!(result, "secret-value");

        env::remove_var("TEST_SECRET_FILE");
    }

    #[test]
    fn test_read_secret_from_env() {
        env::remove_var("TEST_SECRET2_FILE");
        env::set_var("TEST_SECRET2", "env-value");

        let result = read_secret("TEST_SECRET2").unwrap();
        assert_eq!(result, "env-value");

        env::remove_var("TEST_SECRET2");
    }

    #[test]
    fn test_read_secret_from_env_converts_escaped_newlines() {
        // Backward compatibility: \n escape sequences in env vars are converted to real newlines
        // (needed for PEM keys stored as single-line env vars)
        env::remove_var("TEST_SECRET_NEWLINE_FILE");
        env::set_var("TEST_SECRET_NEWLINE", "line1\\nline2\\nline3");

        let result = read_secret("TEST_SECRET_NEWLINE").unwrap();
        assert_eq!(result, "line1\nline2\nline3");

        env::remove_var("TEST_SECRET_NEWLINE");
    }

    #[test]
    fn test_read_secret_file_takes_precedence() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "file-value").unwrap();

        env::set_var("TEST_SECRET3_FILE", file.path());
        env::set_var("TEST_SECRET3", "env-value");

        let result = read_secret("TEST_SECRET3").unwrap();
        assert_eq!(result, "file-value");

        env::remove_var("TEST_SECRET3_FILE");
        env::remove_var("TEST_SECRET3");
    }

    #[test]
    fn test_read_secret_trims_trailing_whitespace() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "secret-value\n").unwrap();

        env::set_var("TEST_SECRET4_FILE", file.path());

        let result = read_secret("TEST_SECRET4").unwrap();
        assert_eq!(result, "secret-value");

        env::remove_var("TEST_SECRET4_FILE");
    }

    #[test]
    fn test_read_secret_preserves_internal_newlines() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "line1\nline2\nline3").unwrap();

        env::set_var("TEST_SECRET5_FILE", file.path());

        let result = read_secret("TEST_SECRET5").unwrap();
        assert_eq!(result, "line1\nline2\nline3");

        env::remove_var("TEST_SECRET5_FILE");
    }

    #[test]
    fn test_read_secret_optional_none_when_missing() {
        env::remove_var("TEST_OPT_SECRET_FILE");
        env::remove_var("TEST_OPT_SECRET");

        let result = read_secret_optional("TEST_OPT_SECRET").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_read_secret_optional_none_when_empty() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "   ").unwrap();

        env::set_var("TEST_OPT_SECRET2_FILE", file.path());

        let result = read_secret_optional("TEST_OPT_SECRET2").unwrap();
        assert_eq!(result, None);

        env::remove_var("TEST_OPT_SECRET2_FILE");
    }

    #[test]
    fn test_read_secret_optional_returns_value() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "optional-secret").unwrap();

        env::set_var("TEST_OPT_SECRET3_FILE", file.path());

        let result = read_secret_optional("TEST_OPT_SECRET3").unwrap();
        assert_eq!(result, Some("optional-secret".to_string()));

        env::remove_var("TEST_OPT_SECRET3_FILE");
    }

    #[test]
    fn test_read_secret_optional_errors_when_file_unreadable() {
        // If _FILE is explicitly set but the file doesn't exist, that's an error
        // (not a silent fallback to None)
        env::set_var("TEST_OPT_SECRET4_FILE", "/nonexistent/path/to/secret");
        env::remove_var("TEST_OPT_SECRET4");

        let result = read_secret_optional("TEST_OPT_SECRET4");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to read TEST_OPT_SECRET4 from file"));

        env::remove_var("TEST_OPT_SECRET4_FILE");
    }

    /// Regression test: OPENAI_WEBHOOK_SECRET must support the _FILE pattern.
    ///
    /// File-based secret deployments (e.g., Kubernetes secrets mounted as files)
    /// should be able to configure the OpenAI webhook secret using
    /// OPENAI_WEBHOOK_SECRET_FILE. Without this support, the /openai-webhook
    /// endpoint silently returns 503, which is a likely misconfiguration.
    #[test]
    fn test_openai_webhook_secret_supports_file_pattern() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "whsec_test_secret_from_file").unwrap();

        env::set_var("OPENAI_WEBHOOK_SECRET_FILE", file.path());
        env::remove_var("OPENAI_WEBHOOK_SECRET");

        let result = read_secret_optional("OPENAI_WEBHOOK_SECRET").unwrap();
        assert_eq!(
            result,
            Some("whsec_test_secret_from_file".to_string()),
            "OPENAI_WEBHOOK_SECRET should be readable from OPENAI_WEBHOOK_SECRET_FILE"
        );

        env::remove_var("OPENAI_WEBHOOK_SECRET_FILE");
    }
}
