use anyhow::Result;
use axum::{
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
use robocop_server::webhook::webhook_router;
use robocop_server::{AppState, RecordingLogger};

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
                "RECORDING_ENABLED (default: false)",
                "RECORDING_LOG_PATH (default: recordings.jsonl)"
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

    let app_state = Arc::new(AppState {
        github_client,
        openai_client,
        webhook_secret: config.github_webhook_secret,
        target_user_id: config.target_user_id,
        pending_batches: Arc::new(RwLock::new(HashMap::new())),
        review_states: Arc::new(RwLock::new(HashMap::new())),
        recording_logger,
    });

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/help", get(help_handler))
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
