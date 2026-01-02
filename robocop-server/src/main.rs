use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, info, Level};

use robocop_server::batch_processor::batch_polling_loop;
use robocop_server::config::Config;
use robocop_server::github::GitHubClient;
use robocop_server::openai::OpenAIClient;
use robocop_server::reconciliation::reconcile_orphaned_batches;
use robocop_server::state_machine::repository::SqliteRepository;
use robocop_server::webhook::webhook_router;
use robocop_server::{AppState, RecordingLogger, StateStore};

async fn health_check() -> Result<Json<serde_json::Value>, StatusCode> {
    Ok(Json(json!({
        "status": "healthy",
        "service": "robocop"
    })))
}

async fn help_handler(headers: HeaderMap) -> Response {
    // Check Accept header for content negotiation
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    // If client prefers HTML, serve HTML (case-insensitive check)
    if accept.to_lowercase().contains("text/html") {
        let html = generate_help_html();
        return Html(html).into_response();
    }

    // Default to JSON
    let version = robocop_server::get_bot_version();
    let json_data = json!({
        "service": "robocop",
        "version": version,
        "description": "Automated code reviews using OpenAI's batch API",
        "endpoints": [
            {
                "path": "/health",
                "method": "GET",
                "description": "Health check endpoint",
                "authentication": "None",
                "response_format": "application/json"
            },
            {
                "path": "/webhook",
                "method": "POST",
                "description": "GitHub webhook receiver for PR events",
                "authentication": "GitHub webhook signature (X-Hub-Signature-256)",
                "response_format": "application/json"
            },
            {
                "path": "/help",
                "method": "GET",
                "description": "API documentation and service information",
                "authentication": "None",
                "response_format": "Supports content negotiation (JSON/HTML)"
            },
            {
                "path": "/status",
                "method": "GET",
                "description": "Status dashboard showing all tracked PRs and their review states",
                "authentication": "Bearer token (Authorization: Bearer <STATUS_AUTH_TOKEN>)",
                "response_format": "Supports content negotiation (JSON/HTML)"
            }
        ],
        "features": [
            "Automated code reviews on PR open/synchronize events",
            "OpenAI batch API integration for cost-effective processing",
            "Superseded commit cancellation using git ancestry",
            "Review status tracking and updates via PR comments",
            "Review suppression via PR description or commands",
            "Manual review trigger via @smaug123-robocop review comment",
            "Enable/disable reviews via @smaug123-robocop enable-reviews/disable-reviews",
            "Cancel pending reviews via @smaug123-robocop cancel comment"
        ],
        "configuration": {
            "required_env_vars": [
                "GITHUB_APP_ID",
                "GITHUB_PRIVATE_KEY",
                "GITHUB_WEBHOOK_SECRET",
                "OPENAI_API_KEY",
                "TARGET_GITHUB_USER_ID"
            ],
            "optional_env_vars": [
                "PORT (default: 3000)",
                "STATE_DIR (default: current directory)",
                "RECORDING_ENABLED (default: false)",
                "RECORDING_LOG_PATH (default: recordings.jsonl)",
                "STATUS_AUTH_TOKEN (required to enable /status endpoint)"
            ]
        },
        "documentation": "https://github.com/Smaug123/robocop"
    });

    Json(json_data).into_response()
}

fn generate_help_html() -> String {
    const HELP_HTML_TEMPLATE: &str = include_str!("help.html");
    let version = robocop_server::get_bot_version();
    HELP_HTML_TEMPLATE.replace("{version}", &version)
}

/// Authentication error types for the /status endpoint.
enum StatusAuthError {
    /// Token not configured - endpoint is disabled
    Disabled,
    /// Token configured but request has invalid/missing auth
    Unauthorized,
}

impl StatusAuthError {
    fn status_code(&self) -> StatusCode {
        match self {
            StatusAuthError::Disabled => StatusCode::FORBIDDEN,
            StatusAuthError::Unauthorized => StatusCode::UNAUTHORIZED,
        }
    }
}

/// Validate bearer token authentication for the /status endpoint.
///
/// Returns `Ok(())` if authentication is valid, or `Err(StatusAuthError)` if not.
fn validate_status_auth(
    headers: &HeaderMap,
    expected_token: &Option<String>,
) -> Result<(), StatusAuthError> {
    let expected = match expected_token {
        Some(token) => token,
        None => return Err(StatusAuthError::Disabled),
    };

    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(value) if value.starts_with("Bearer ") => {
            let provided_token = &value[7..]; // Skip "Bearer "
                                              // Use constant-time comparison to prevent timing attacks
            if constant_time_eq(provided_token.as_bytes(), expected.as_bytes()) {
                Ok(())
            } else {
                Err(StatusAuthError::Unauthorized)
            }
        }
        _ => Err(StatusAuthError::Unauthorized),
    }
}

/// Constant-time byte comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

async fn status_handler(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response {
    let version = robocop_server::get_bot_version();

    // Check Accept header for content negotiation
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    let prefers_html = accept.to_lowercase().contains("text/html");

    // Validate authentication
    let auth_result = validate_status_auth(&headers, &state.status_auth_token);
    if let Err(response) = auth_result {
        return if prefers_html {
            let error_html = match response {
                StatusAuthError::Disabled => {
                    r#"<!DOCTYPE html>
<html>
<head><title>Robocop Status - Disabled</title></head>
<body>
<h1>Status Endpoint Disabled</h1>
<p>The /status endpoint is disabled. Set STATUS_AUTH_TOKEN to enable it.</p>
</body>
</html>"#
                }
                StatusAuthError::Unauthorized => {
                    r#"<!DOCTYPE html>
<html>
<head><title>Robocop Status - Unauthorized</title></head>
<body>
<h1>Unauthorized</h1>
<p>Valid Authorization header required. Use: Authorization: Bearer &lt;token&gt;</p>
</body>
</html>"#
                }
            };
            (response.status_code(), Html(error_html.to_string())).into_response()
        } else {
            let error_json = match response {
                StatusAuthError::Disabled => json!({"error": "Status endpoint disabled"}),
                StatusAuthError::Unauthorized => json!({"error": "Unauthorized"}),
            };
            (response.status_code(), Json(error_json)).into_response()
        };
    }

    match state.state_store.get_all_states().await {
        Ok(all_states) => {
            let status_data = robocop_server::status::StatusData::from_states(all_states, version);
            if prefers_html {
                Html(generate_status_html(&status_data)).into_response()
            } else {
                Json(status_data).into_response()
            }
        }
        Err(e) => {
            error!("Failed to retrieve states for status endpoint: {}", e);
            if prefers_html {
                // Return an error page for HTML clients
                let error_html = format!(
                    r#"<!DOCTYPE html>
<html>
<head><title>Robocop Status - Error</title></head>
<body>
<h1>Status Unavailable</h1>
<p>Failed to retrieve PR states from the database. Please try again later.</p>
<p>Version: {}</p>
</body>
</html>"#,
                    version
                );
                (StatusCode::INTERNAL_SERVER_ERROR, Html(error_html)).into_response()
            } else {
                // Return a JSON error for API clients
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "Failed to retrieve states",
                        "version": version
                    })),
                )
                    .into_response()
            }
        }
    }
}

/// Escape JSON string for safe embedding in HTML `<script>` tags.
///
/// Standard JSON serialization doesn't escape characters that are safe in JSON
/// but dangerous in HTML context. For example, `</script>` in a JSON string
/// would close the enclosing script tag, enabling XSS attacks.
///
/// This function escapes:
/// - `<` to `\u003c` (prevents `</script>` and `<!--` sequences)
/// - `>` to `\u003e` (prevents `-->` and similar)
/// - `&` to `\u0026` (prevents HTML entity interpretation)
/// - U+2028 (LINE SEPARATOR) to `\u2028` (prevents JS line terminator issues)
/// - U+2029 (PARAGRAPH SEPARATOR) to `\u2029` (prevents JS line terminator issues)
///
/// These escape sequences are valid JSON, so the resulting string remains
/// valid JSON that JavaScript can parse correctly.
fn escape_json_for_script_tag(json: &str) -> String {
    json.replace('&', r"\u0026")
        .replace('<', r"\u003c")
        .replace('>', r"\u003e")
        .replace('\u{2028}', r"\u2028")
        .replace('\u{2029}', r"\u2029")
}

fn generate_status_html(data: &robocop_server::status::StatusData) -> String {
    const STATUS_HTML_TEMPLATE: &str = include_str!("status.html");

    let summary_json = serde_json::to_string(&data.summary).unwrap_or_else(|_| "{}".to_string());
    let prs_json = serde_json::to_string(&data.prs).unwrap_or_else(|_| "[]".to_string());
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();

    // Escape JSON for safe embedding in <script> tags to prevent XSS
    let summary_json = escape_json_for_script_tag(&summary_json);
    let prs_json = escape_json_for_script_tag(&prs_json);

    STATUS_HTML_TEMPLATE
        .replace("{version}", &data.version)
        .replace("{timestamp}", &timestamp)
        .replace("{summary_json}", &summary_json)
        .replace("{prs_json}", &prs_json)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    info!("Starting GitHub Code Review Bot");

    let config =
        Config::from_env().expect("Failed to load configuration from environment variables");

    // Initialize recording logger if enabled
    let recording_logger = if config.recording_enabled {
        match RecordingLogger::new(PathBuf::from(&config.recording_log_path)) {
            Ok(logger) => {
                info!(
                    "Recording enabled, logging to: {}",
                    config.recording_log_path
                );
                Some(logger)
            }
            Err(e) => {
                error!("Failed to initialize recording logger: {}", e);
                None
            }
        }
    } else {
        None
    };

    let github_client = GitHubClient::new_with_recording(
        config.github_app_id,
        config.github_private_key,
        recording_logger
            .as_ref()
            .map(|l: &RecordingLogger| l.clone_for_middleware()),
    );

    let openai_client = OpenAIClient::new_with_recording(
        config.openai_api_key,
        recording_logger
            .as_ref()
            .map(|l: &RecordingLogger| l.clone_for_middleware()),
    );

    let db_path = config.state_dir.join("robocop-state.db");
    info!("Using state database: {}", db_path.display());
    let sqlite_repo =
        SqliteRepository::new(&db_path).expect("Failed to initialize SQLite database");

    // Log warning if status auth token is not configured
    if config.status_auth_token.is_none() {
        info!("STATUS_AUTH_TOKEN not set: /status endpoint is disabled");
    }

    let app_state = Arc::new(AppState {
        github_client: Arc::new(github_client),
        openai_client: Arc::new(openai_client),
        webhook_secret: config.github_webhook_secret,
        target_user_id: config.target_user_id,
        review_states: Arc::new(RwLock::new(HashMap::new())),
        state_store: Arc::new(StateStore::with_repository(Arc::new(sqlite_repo))),
        recording_logger,
        status_auth_token: config.status_auth_token,
    });

    // Run crash recovery reconciliation before accepting any requests
    // This ensures any PRs stuck in BatchSubmitting state are recovered
    reconcile_orphaned_batches(app_state.clone()).await;

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/help", get(help_handler))
        .route("/status", get(status_handler))
        .merge(webhook_router(app_state.clone()))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(app_state.clone());

    // Start the batch polling loop
    let polling_state = app_state.clone();
    tokio::spawn(async move {
        batch_polling_loop(polling_state).await;
    });

    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.port)).await?;
    info!("Server listening on port {}", config.port);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // XSS prevention: escape_json_for_script_tag tests
    // =========================================================================

    #[test]
    fn test_escape_json_for_script_tag_basic() {
        // Simple JSON without dangerous characters
        let json = r#"{"name": "test", "value": 42}"#;
        let escaped = escape_json_for_script_tag(json);
        assert_eq!(escaped, json); // No changes needed
    }

    #[test]
    fn test_escape_json_for_script_tag_closes_script_tag() {
        // XSS payload: closing script tag
        let json = r#"{"error": "</script><script>alert('xss')</script>"}"#;
        let escaped = escape_json_for_script_tag(json);

        // Must not contain literal </script>
        assert!(
            !escaped.contains("</script>"),
            "Escaped JSON must not contain </script>: {}",
            escaped
        );
        // Should contain escaped versions
        assert!(escaped.contains(r"\u003c/script\u003e"));
    }

    #[test]
    fn test_escape_json_for_script_tag_html_comment() {
        // XSS payload: HTML comment that could break out
        let json = r#"{"comment": "<!-- <script>alert('xss')</script> -->"}"#;
        let escaped = escape_json_for_script_tag(json);

        assert!(!escaped.contains("<!--"));
        assert!(!escaped.contains("-->"));
        assert!(escaped.contains(r"\u003c!--"));
    }

    #[test]
    fn test_escape_json_for_script_tag_ampersand() {
        // Ampersand that could be interpreted as HTML entity
        let json = r#"{"text": "a & b"}"#;
        let escaped = escape_json_for_script_tag(json);

        assert!(!escaped.contains(" & "));
        assert!(escaped.contains(r"\u0026"));
    }

    #[test]
    fn test_escape_json_for_script_tag_preserves_json_validity() {
        // The escaped output should still be valid JSON that parses to the same value
        let original = r#"{"error": "</script>", "text": "a & b < c > d"}"#;
        let escaped = escape_json_for_script_tag(original);

        // The escaped version should be parseable
        let parsed: serde_json::Value =
            serde_json::from_str(&escaped).expect("Escaped JSON should still be valid JSON");

        // The parsed values should contain the original text
        assert_eq!(parsed["error"].as_str().unwrap(), "</script>");
        assert_eq!(parsed["text"].as_str().unwrap(), "a & b < c > d");
    }

    #[test]
    fn test_escape_json_for_script_tag_line_separator() {
        // U+2028 (LINE SEPARATOR) is valid in JSON but breaks JavaScript parsing
        // when embedded in <script> tags because JS treats it as a line terminator
        let json = "{\"text\": \"line1\u{2028}line2\"}";
        let escaped = escape_json_for_script_tag(json);

        // Must not contain literal U+2028
        assert!(
            !escaped.contains('\u{2028}'),
            "Escaped JSON must not contain U+2028: {}",
            escaped
        );
        // Should contain escaped version
        assert!(escaped.contains(r"\u2028"));

        // Verify it's still valid JSON that parses correctly
        let parsed: serde_json::Value =
            serde_json::from_str(&escaped).expect("Escaped JSON should still be valid JSON");
        assert_eq!(parsed["text"].as_str().unwrap(), "line1\u{2028}line2");
    }

    #[test]
    fn test_escape_json_for_script_tag_paragraph_separator() {
        // U+2029 (PARAGRAPH SEPARATOR) is valid in JSON but breaks JavaScript parsing
        // when embedded in <script> tags because JS treats it as a line terminator
        let json = "{\"text\": \"para1\u{2029}para2\"}";
        let escaped = escape_json_for_script_tag(json);

        // Must not contain literal U+2029
        assert!(
            !escaped.contains('\u{2029}'),
            "Escaped JSON must not contain U+2029: {}",
            escaped
        );
        // Should contain escaped version
        assert!(escaped.contains(r"\u2029"));

        // Verify it's still valid JSON that parses correctly
        let parsed: serde_json::Value =
            serde_json::from_str(&escaped).expect("Escaped JSON should still be valid JSON");
        assert_eq!(parsed["text"].as_str().unwrap(), "para1\u{2029}para2");
    }

    // =========================================================================
    // Authentication: validate_status_auth tests
    // =========================================================================

    fn make_headers_with_auth(auth_value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            auth_value.parse().unwrap(),
        );
        headers
    }

    #[test]
    fn test_auth_disabled_when_no_token_configured() {
        let headers = HeaderMap::new();
        let token: Option<String> = None;

        let result = validate_status_auth(&headers, &token);
        assert!(matches!(result, Err(StatusAuthError::Disabled)));
    }

    #[test]
    fn test_auth_unauthorized_when_no_header() {
        let headers = HeaderMap::new();
        let token = Some("secret-token".to_string());

        let result = validate_status_auth(&headers, &token);
        assert!(matches!(result, Err(StatusAuthError::Unauthorized)));
    }

    #[test]
    fn test_auth_unauthorized_when_wrong_token() {
        let headers = make_headers_with_auth("Bearer wrong-token");
        let token = Some("secret-token".to_string());

        let result = validate_status_auth(&headers, &token);
        assert!(matches!(result, Err(StatusAuthError::Unauthorized)));
    }

    #[test]
    fn test_auth_unauthorized_when_not_bearer() {
        let headers = make_headers_with_auth("Basic dXNlcjpwYXNz");
        let token = Some("secret-token".to_string());

        let result = validate_status_auth(&headers, &token);
        assert!(matches!(result, Err(StatusAuthError::Unauthorized)));
    }

    #[test]
    fn test_auth_success_with_valid_token() {
        let headers = make_headers_with_auth("Bearer secret-token");
        let token = Some("secret-token".to_string());

        let result = validate_status_auth(&headers, &token);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auth_case_sensitive() {
        // Bearer prefix is case-sensitive per RFC 7235
        let headers = make_headers_with_auth("bearer secret-token"); // lowercase
        let token = Some("secret-token".to_string());

        let result = validate_status_auth(&headers, &token);
        assert!(matches!(result, Err(StatusAuthError::Unauthorized)));
    }

    // =========================================================================
    // Constant time comparison tests
    // =========================================================================

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"a", b"a"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellO"));
        assert!(!constant_time_eq(b"a", b"b"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"hello", b"hello!"));
        assert!(!constant_time_eq(b"hello!", b"hello"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    // =========================================================================
    // StatusAuthError status codes
    // =========================================================================

    #[test]
    fn test_status_auth_error_codes() {
        assert_eq!(
            StatusAuthError::Disabled.status_code(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            StatusAuthError::Unauthorized.status_code(),
            StatusCode::UNAUTHORIZED
        );
    }

    /// Documents why config.rs must filter out empty STATUS_AUTH_TOKEN values.
    ///
    /// If an empty string token were allowed through config parsing,
    /// `Authorization: Bearer ` (with trailing space but no token) would authenticate
    /// successfully, effectively making /status unauthenticated.
    #[test]
    fn test_empty_token_would_be_vulnerable() {
        // This demonstrates the vulnerability that config.rs prevents by filtering empty strings
        let headers = make_headers_with_auth("Bearer "); // Note: "Bearer " with trailing space
        let token = Some("".to_string()); // Empty token (config.rs prevents this)

        // Empty provided token matches empty expected token - this would be a security hole!
        let result = validate_status_auth(&headers, &token);
        assert!(
            result.is_ok(),
            "Empty token matches empty bearer - config.rs must filter empty STATUS_AUTH_TOKEN"
        );
    }
}
