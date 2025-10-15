use anyhow::Result;
use axum::{http::StatusCode, response::Json, routing::get, Router};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, info, Level};

use github_bot::batch_processor::batch_polling_loop;
use github_bot::config::Config;
use github_bot::github::GitHubClient;
use github_bot::openai::OpenAIClient;
use github_bot::recording::RecordingLogger;
use github_bot::webhook::webhook_router;
use github_bot::AppState;

async fn health_check() -> Result<Json<serde_json::Value>, StatusCode> {
    Ok(Json(json!({
        "status": "healthy",
        "service": "github-bot"
    })))
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
        recording_logger.as_ref().map(|l| l.clone_for_middleware()),
    );

    let openai_client = OpenAIClient::new_with_recording(
        config.openai_api_key,
        recording_logger.as_ref().map(|l| l.clone_for_middleware()),
    );

    let app_state = Arc::new(AppState {
        github_client,
        openai_client,
        webhook_secret: config.github_webhook_secret,
        target_user_id: config.target_user_id,
        pending_batches: Arc::new(RwLock::new(HashMap::new())),
        recording_logger,
    });

    let app = Router::new()
        .route("/health", get(health_check))
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
