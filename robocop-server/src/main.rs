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
                "STATE_DIR (default: current directory)",
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

async fn status_handler(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response {
    let all_states = state.state_store.get_all_states().await;
    let version = robocop_server::get_bot_version();
    let status_data = robocop_server::status::StatusData::from_states(all_states, version);

    // Check Accept header for content negotiation
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    // If client prefers HTML, serve HTML
    if accept.to_lowercase().contains("text/html") {
        let html = generate_status_html(&status_data);
        return Html(html).into_response();
    }

    // Default to JSON
    Json(status_data).into_response()
}

fn generate_status_html(data: &robocop_server::status::StatusData) -> String {
    const STATUS_HTML_TEMPLATE: &str = include_str!("status.html");

    let summary_json = serde_json::to_string(&data.summary).unwrap_or_else(|_| "{}".to_string());
    let prs_json = serde_json::to_string(&data.prs).unwrap_or_else(|_| "[]".to_string());
    let timestamp = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();

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

    let app_state = Arc::new(AppState {
        github_client: Arc::new(github_client),
        openai_client: Arc::new(openai_client),
        webhook_secret: config.github_webhook_secret,
        target_user_id: config.target_user_id,
        review_states: Arc::new(RwLock::new(HashMap::new())),
        state_store: Arc::new(StateStore::with_repository(Arc::new(sqlite_repo))),
        recording_logger,
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
