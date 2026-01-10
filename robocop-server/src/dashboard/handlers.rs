//! HTTP handlers for the dashboard API.
//!
//! These handlers provide JSON APIs for querying PR states and events,
//! as well as serving the dashboard HTML.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::sync::Arc;

use crate::AppState;

/// Number of days of activity to show in the dashboard.
const RECENT_ACTIVITY_DAYS: i64 = 7;

/// Maximum number of events to return per PR.
const MAX_EVENTS_PER_PR: usize = 100;

/// API response for the PR list endpoint.
#[derive(Debug, Serialize)]
pub struct PrsApiResponse {
    /// Version of the API response format.
    pub version: String,
    /// List of PRs with recent activity.
    pub prs: Vec<PrSummaryResponse>,
}

/// Summary of a PR for the list view.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummaryResponse {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub current_state: String,
    pub latest_event_at: i64,
    pub event_count: usize,
    pub reviews_enabled: bool,
}

/// API response for the PR events endpoint.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrEventsApiResponse {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub current_state: String,
    pub reviews_enabled: bool,
    pub events: Vec<PrEventResponse>,
}

/// Single event in the timeline.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrEventResponse {
    pub id: i64,
    pub event_type: super::types::DashboardEventType,
    pub recorded_at: i64,
}

/// Validate the authorization header against the status auth token.
///
/// Returns `Ok(())` if authorized, or an error response if not.
#[allow(clippy::result_large_err)] // Response is large but this is idiomatic in Axum handlers
fn validate_auth(headers: &HeaderMap, auth_token: &Option<String>) -> Result<(), Response> {
    // If no auth token is configured, the endpoint is disabled
    let Some(expected_token) = auth_token else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Dashboard API is disabled (STATUS_AUTH_TOKEN not configured)",
        )
            .into_response());
    };

    // Check for Authorization header
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(value) if value.starts_with("Bearer ") => {
            let provided_token = &value[7..];
            if provided_token == expected_token {
                Ok(())
            } else {
                Err((StatusCode::UNAUTHORIZED, "Invalid token").into_response())
            }
        }
        Some(_) => Err((
            StatusCode::UNAUTHORIZED,
            "Invalid Authorization header format. Expected: Bearer <token>",
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header. Expected: Bearer <token>",
        )
            .into_response()),
    }
}

/// Handler: GET /dashboard
///
/// Returns the dashboard HTML page. This endpoint does NOT require authentication
/// since the HTML itself doesn't contain sensitive data - the JavaScript will
/// prompt for the auth token to make API calls.
pub async fn get_dashboard_html() -> impl IntoResponse {
    // For now, return a placeholder. The actual HTML will be added in Stage 4.
    Html(include_str!("dashboard.html"))
}

/// Handler: GET /dashboard/api/prs
///
/// Returns a list of PRs with recent activity (last 7 days).
/// Requires Bearer token authentication via STATUS_AUTH_TOKEN.
pub async fn get_prs_api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<PrsApiResponse>, Response> {
    validate_auth(&headers, &state.status_auth_token)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since_timestamp = now - (RECENT_ACTIVITY_DAYS * 24 * 60 * 60);

    let summaries = state
        .state_store
        .get_prs_with_recent_activity(since_timestamp)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get PRs with recent activity: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to query PR activity",
            )
                .into_response()
        })?;

    let prs = summaries
        .into_iter()
        .map(|s| PrSummaryResponse {
            repo_owner: s.repo_owner,
            repo_name: s.repo_name,
            pr_number: s.pr_number,
            current_state: s.current_state,
            latest_event_at: s.latest_event_at,
            event_count: s.event_count,
            reviews_enabled: s.reviews_enabled,
        })
        .collect();

    Ok(Json(PrsApiResponse {
        version: crate::get_bot_version(),
        prs,
    }))
}

/// Handler: GET /dashboard/api/prs/:owner/:repo/:pr_number/events
///
/// Returns the event timeline for a specific PR.
/// Requires Bearer token authentication via STATUS_AUTH_TOKEN.
pub async fn get_pr_events_api(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, pr_number)): Path<(String, String, u64)>,
) -> Result<Json<PrEventsApiResponse>, Response> {
    validate_auth(&headers, &state.status_auth_token)?;

    let pr_id = crate::StateMachinePrId::new(&owner, &repo, pr_number);

    // Get events from the repository
    let events = state
        .state_store
        .get_pr_events(&pr_id, MAX_EVENTS_PER_PR)
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to get PR events for {}/{} #{}: {}",
                owner,
                repo,
                pr_number,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to query PR events",
            )
                .into_response()
        })?;

    // Get current state from the state store
    // Note: StateStore::get returns Option<ReviewMachineState>, not Result
    let current_state_opt = state.state_store.get(&pr_id).await;

    let (current_state, reviews_enabled) = match current_state_opt {
        Some(machine_state) => (state_name(&machine_state), machine_state.reviews_enabled()),
        None => ("Unknown".to_string(), true),
    };

    let events = events
        .into_iter()
        .map(|e| PrEventResponse {
            id: e.id,
            event_type: e.event_type,
            recorded_at: e.recorded_at,
        })
        .collect();

    Ok(Json(PrEventsApiResponse {
        repo_owner: owner,
        repo_name: repo,
        pr_number,
        current_state,
        reviews_enabled,
        events,
    }))
}

/// Extract a display name from a ReviewMachineState.
fn state_name(state: &crate::state_machine::state::ReviewMachineState) -> String {
    use crate::state_machine::state::ReviewMachineState;
    match state {
        ReviewMachineState::Idle { .. } => "Idle".to_string(),
        ReviewMachineState::Preparing { .. } => "Preparing".to_string(),
        ReviewMachineState::BatchSubmitting { .. } => "BatchSubmitting".to_string(),
        ReviewMachineState::AwaitingAncestryCheck { .. } => "AwaitingAncestryCheck".to_string(),
        ReviewMachineState::BatchPending { .. } => "BatchPending".to_string(),
        ReviewMachineState::Completed { .. } => "Completed".to_string(),
        ReviewMachineState::Failed { .. } => "Failed".to_string(),
        ReviewMachineState::Cancelled { .. } => "Cancelled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_validate_auth_success() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-token-123"),
        );
        let auth_token = Some("test-token-123".to_string());

        let result = validate_auth(&headers, &auth_token);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_auth_wrong_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong-token"),
        );
        let auth_token = Some("test-token-123".to_string());

        let result = validate_auth(&headers, &auth_token);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_auth_missing_header() {
        let headers = HeaderMap::new();
        let auth_token = Some("test-token-123".to_string());

        let result = validate_auth(&headers, &auth_token);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_auth_disabled() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer test-token"),
        );
        let auth_token = None;

        let result = validate_auth(&headers, &auth_token);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_auth_invalid_format() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        let auth_token = Some("test-token-123".to_string());

        let result = validate_auth(&headers, &auth_token);
        assert!(result.is_err());
    }
}
