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

async fn help_handler(headers: HeaderMap) -> Response {
    // Check Accept header for content negotiation
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");

    // If client prefers HTML, serve HTML
    if accept.contains("text/html") {
        let html = generate_help_html();
        return Html(html).into_response();
    }

    // Default to JSON
    let version = github_bot::get_bot_version();
    let json_data = json!({
        "service": "GitHub Code Review Bot",
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
            "Manual review trigger via @robocop review comment",
            "Cancel pending reviews via @robocop cancel comment"
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
        "documentation": "https://github.com/your-repo/robocop"
    });

    Json(json_data).into_response()
}

fn generate_help_html() -> String {
    let version = github_bot::get_bot_version();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Robocop GitHub Bot - Help</title>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif;
            line-height: 1.6;
            max-width: 900px;
            margin: 0 auto;
            padding: 20px;
            background-color: #f5f5f5;
        }}
        .container {{
            background: white;
            padding: 40px;
            border-radius: 8px;
            box-shadow: 0 2px 4px rgba(0,0,0,0.1);
        }}
        h1 {{
            color: #2c3e50;
            border-bottom: 3px solid #3498db;
            padding-bottom: 10px;
        }}
        h2 {{
            color: #34495e;
            margin-top: 30px;
            border-bottom: 1px solid #ecf0f1;
            padding-bottom: 5px;
        }}
        .version {{
            color: #7f8c8d;
            font-size: 0.9em;
        }}
        .endpoint {{
            background: #ecf0f1;
            padding: 15px;
            margin: 10px 0;
            border-radius: 5px;
            border-left: 4px solid #3498db;
        }}
        .endpoint-path {{
            font-family: 'Courier New', monospace;
            font-weight: bold;
            color: #2980b9;
        }}
        .method {{
            display: inline-block;
            background: #27ae60;
            color: white;
            padding: 2px 8px;
            border-radius: 3px;
            font-size: 0.85em;
            font-weight: bold;
            margin-right: 8px;
        }}
        .method.post {{
            background: #e67e22;
        }}
        ul {{
            line-height: 1.8;
        }}
        code {{
            background: #f8f9fa;
            padding: 2px 6px;
            border-radius: 3px;
            font-family: 'Courier New', monospace;
            color: #e74c3c;
        }}
        .feature-list li {{
            margin: 8px 0;
        }}
        .env-var {{
            background: #fff3cd;
            padding: 10px;
            margin: 5px 0;
            border-radius: 4px;
            font-family: 'Courier New', monospace;
            font-size: 0.9em;
        }}
        footer {{
            margin-top: 40px;
            padding-top: 20px;
            border-top: 1px solid #ecf0f1;
            text-align: center;
            color: #7f8c8d;
            font-size: 0.9em;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h1>🤖 Robocop GitHub Bot</h1>
        <p class="version">Version: {version}</p>
        <p><strong>Automated code reviews using OpenAI's batch API</strong></p>

        <h2>📡 Available Endpoints</h2>

        <div class="endpoint">
            <div><span class="method">GET</span><span class="endpoint-path">/health</span></div>
            <p>Health check endpoint - returns service status</p>
            <p><strong>Authentication:</strong> None</p>
        </div>

        <div class="endpoint">
            <div><span class="method post">POST</span><span class="endpoint-path">/webhook</span></div>
            <p>GitHub webhook receiver for PR events (opened, synchronize)</p>
            <p><strong>Authentication:</strong> GitHub webhook signature (<code>X-Hub-Signature-256</code>)</p>
        </div>

        <div class="endpoint">
            <div><span class="method">GET</span><span class="endpoint-path">/help</span></div>
            <p>This page - API documentation and service information</p>
            <p><strong>Authentication:</strong> None</p>
            <p><strong>Content Negotiation:</strong> Returns JSON for API clients, HTML for browsers</p>
        </div>

        <h2>✨ Features</h2>
        <ul class="feature-list">
            <li>Automated code reviews on PR open/synchronize events</li>
            <li>OpenAI batch API integration for cost-effective processing</li>
            <li>Superseded commit cancellation using git ancestry</li>
            <li>Review status tracking and updates via PR comments</li>
            <li>Manual review trigger via <code>@robocop review</code> comment</li>
            <li>Cancel pending reviews via <code>@robocop cancel</code> comment</li>
        </ul>

        <h2>⚙️ Configuration</h2>
        <h3>Required Environment Variables</h3>
        <div class="env-var">GITHUB_APP_ID</div>
        <div class="env-var">GITHUB_PRIVATE_KEY</div>
        <div class="env-var">GITHUB_WEBHOOK_SECRET</div>
        <div class="env-var">OPENAI_API_KEY</div>
        <div class="env-var">TARGET_GITHUB_USER_ID</div>

        <h3>Optional Environment Variables</h3>
        <div class="env-var">PORT (default: 3000)</div>
        <div class="env-var">RECORDING_ENABLED (default: false)</div>
        <div class="env-var">RECORDING_LOG_PATH (default: recordings.jsonl)</div>

        <h2>🔧 Usage</h2>
        <p><strong>Automatic Reviews:</strong> Configure the GitHub webhook to point to <code>/webhook</code>. Reviews will automatically trigger on PR open and new commits.</p>
        <p><strong>Manual Reviews:</strong> Comment <code>@robocop review</code> on any PR to trigger a review.</p>
        <p><strong>Cancel Reviews:</strong> Comment <code>@robocop cancel</code> on any PR to cancel pending reviews.</p>

        <footer>
            <p>Robocop GitHub Bot v{version}</p>
        </footer>
    </div>
</body>
</html>"#,
        version = version
    )
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
