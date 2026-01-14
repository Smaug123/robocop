//! Dashboard event logging operations for SQLite repository.
//!
//! This module handles storing and retrieving PR events for the dashboard
//! timeline view. Events are stored in the `pr_events` table with JSON-encoded
//! event data.

use rusqlite::params;

use super::super::{DashboardEventType, PrEvent, PrSummary, RepositoryError};
use super::{i64_to_pr_number, pr_number_to_i64, usize_to_i64_limit, SqliteRepository};
use crate::state_machine::state::{state_variant_name, ReviewMachineState};
use crate::state_machine::store::StateMachinePrId;

/// Extract the variant name from a DashboardEventType for storage.
fn event_type_variant_name(event_type: &DashboardEventType) -> &'static str {
    match event_type {
        DashboardEventType::WebhookReceived { .. } => "WebhookReceived",
        DashboardEventType::CommandReceived { .. } => "CommandReceived",
        DashboardEventType::StateTransition { .. } => "StateTransition",
        DashboardEventType::BatchSubmitted { .. } => "BatchSubmitted",
        DashboardEventType::BatchCompleted { .. } => "BatchCompleted",
        DashboardEventType::BatchFailed { .. } => "BatchFailed",
        DashboardEventType::BatchCancelled { .. } => "BatchCancelled",
        DashboardEventType::CommentPosted { .. } => "CommentPosted",
        DashboardEventType::CheckRunCreated { .. } => "CheckRunCreated",
    }
}

/// Safely convert an i64 count from SQLite to usize.
///
/// Returns None if the value is negative or exceeds usize::MAX (possible on
/// 32-bit platforms with very large counts). This uses checked conversion
/// instead of `as usize` which would silently wrap/truncate.
fn i64_to_event_count(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

// =============================================================================
// Async implementations
// =============================================================================

impl SqliteRepository {
    pub(super) async fn log_event_impl(&self, event: &PrEvent) -> Result<(), RepositoryError> {
        let conn = self.conn.clone();
        let repo_owner = event.repo_owner.clone();
        let repo_name = event.repo_name.clone();
        let pr_number = pr_number_to_i64(event.pr_number, "log_event")?;
        let recorded_at = event.recorded_at;

        // Serialize event_type to JSON for the event_data column
        let event_json = serde_json::to_string(&event.event_type)
            .map_err(|e| RepositoryError::storage("log_event serialize", e.to_string()))?;

        // Extract the variant name for the event_type column (for easier querying)
        let event_type_name = event_type_variant_name(&event.event_type);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.execute(
                "INSERT INTO pr_events (repo_owner, repo_name, pr_number, event_type, event_data, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    repo_owner,
                    repo_name,
                    pr_number,
                    event_type_name,
                    event_json,
                    recorded_at
                ],
            )
            .map_err(|e| RepositoryError::storage("log_event", e.to_string()))?;

            Ok(())
        })
        .await
        .map_err(|e| RepositoryError::storage("log_event", e.to_string()))?
    }

    pub(super) async fn get_pr_events_impl(
        &self,
        pr_id: &StateMachinePrId,
        limit: usize,
    ) -> Result<Vec<PrEvent>, RepositoryError> {
        let conn = self.conn.clone();
        let repo_owner = pr_id.repo_owner.clone();
        let repo_name = pr_id.repo_name.clone();
        let pr_number = pr_number_to_i64(pr_id.pr_number, "get_pr_events")?;
        let limit_i64 = usize_to_i64_limit(limit, "get_pr_events")?;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn
                .prepare(
                    "SELECT id, repo_owner, repo_name, pr_number, event_data, recorded_at
                     FROM pr_events
                     WHERE repo_owner = ?1 AND repo_name = ?2 AND pr_number = ?3
                     ORDER BY recorded_at DESC, id DESC
                     LIMIT ?4",
                )
                .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?;

            let rows = stmt
                .query_map(
                    params![repo_owner, repo_name, pr_number, limit_i64],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?;

            let mut events = Vec::new();
            for row in rows {
                let (id, owner, name, pr_num, event_data, recorded_at) =
                    row.map_err(|e| RepositoryError::storage("get_pr_events row", e.to_string()))?;

                let event_type: DashboardEventType = serde_json::from_str(&event_data)
                    .map_err(|_| RepositoryError::corruption("event_data JSON"))?;

                let pr_number = i64_to_pr_number(pr_num, "get_pr_events")?;

                events.push(PrEvent {
                    id,
                    repo_owner: owner,
                    repo_name: name,
                    pr_number,
                    event_type,
                    recorded_at,
                });
            }

            Ok(events)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_pr_events", e.to_string()))?
    }

    pub(super) async fn get_prs_with_recent_activity_impl(
        &self,
        since_timestamp: i64,
    ) -> Result<Vec<PrSummary>, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Query to get PR summaries with recent events, joined with pr_states for current state.
            // Uses HAVING instead of WHERE so we count ALL events for each PR,
            // while still filtering to only include PRs that have recent activity.
            let mut stmt = conn
                .prepare(
                    "SELECT
                        e.repo_owner,
                        e.repo_name,
                        e.pr_number,
                        COALESCE(s.state_json, '{}') as state_json,
                        MAX(e.recorded_at) as latest_event_at,
                        COUNT(*) as event_count
                     FROM pr_events e
                     LEFT JOIN pr_states s ON
                        e.repo_owner = s.repo_owner AND
                        e.repo_name = s.repo_name AND
                        e.pr_number = s.pr_number
                     GROUP BY e.repo_owner, e.repo_name, e.pr_number
                     HAVING MAX(e.recorded_at) >= ?1
                     ORDER BY latest_event_at DESC",
                )
                .map_err(|e| {
                    RepositoryError::storage("get_prs_with_recent_activity", e.to_string())
                })?;

            let rows = stmt
                .query_map(params![since_timestamp], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(|e| {
                    RepositoryError::storage("get_prs_with_recent_activity", e.to_string())
                })?;

            let mut summaries = Vec::new();
            for row in rows {
                let (owner, name, pr_num, state_json, latest_event_at, event_count) =
                    row.map_err(|e| {
                        RepositoryError::storage("get_prs_with_recent_activity row", e.to_string())
                    })?;

                // Safely convert PR number from i64 - skip rows with invalid data
                let pr_number = match i64_to_pr_number(pr_num, "get_prs_with_recent_activity") {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(
                            "Skipping PR with invalid number in {}/{}: {}",
                            owner,
                            name,
                            e
                        );
                        continue;
                    }
                };

                // Parse state to extract current_state name and reviews_enabled
                let (current_state, reviews_enabled) = if state_json == "{}" {
                    ("Unknown".to_string(), true)
                } else {
                    match serde_json::from_str::<ReviewMachineState>(&state_json) {
                        Ok(state) => (state_variant_name(&state), state.reviews_enabled()),
                        Err(_) => ("Unknown".to_string(), true),
                    }
                };

                // Safely convert event count from i64 - skip rows with invalid data
                let event_count = match i64_to_event_count(event_count) {
                    Some(n) => n,
                    None => {
                        tracing::warn!(
                            "Skipping PR {}/{} #{} with invalid event count: {}",
                            owner,
                            name,
                            pr_number,
                            event_count
                        );
                        continue;
                    }
                };

                summaries.push(PrSummary {
                    repo_owner: owner,
                    repo_name: name,
                    pr_number,
                    current_state,
                    latest_event_at,
                    event_count,
                    reviews_enabled,
                });
            }

            Ok(summaries)
        })
        .await
        .map_err(|e| RepositoryError::storage("get_prs_with_recent_activity", e.to_string()))?
    }

    pub(super) async fn cleanup_old_events_impl(
        &self,
        older_than: i64,
    ) -> Result<usize, RepositoryError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let deleted = conn
                .execute(
                    "DELETE FROM pr_events WHERE recorded_at < ?1",
                    params![older_than],
                )
                .map_err(|e| RepositoryError::storage("cleanup_old_events", e.to_string()))?;

            Ok(deleted)
        })
        .await
        .map_err(|e| RepositoryError::storage("cleanup_old_events", e.to_string()))?
    }
}
