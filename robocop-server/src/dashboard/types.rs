//! Types for the dashboard event logging system.
//!
//! These types represent events that occur during PR review processing,
//! enabling a detailed timeline view in the dashboard.

use serde::{Deserialize, Serialize};

/// Event types that can be logged for dashboard display.
///
/// This enum uses serde's tagged representation for clean JSON serialization:
/// ```json
/// { "type": "StateTransition", "data": { "from_state": "Idle", ... } }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEventType {
    /// A webhook was received from GitHub.
    WebhookReceived {
        /// The webhook action (e.g., "opened", "synchronize", "edited").
        action: String,
        /// The head SHA at the time of the webhook.
        head_sha: String,
    },

    /// A command was received via PR comment.
    CommandReceived {
        /// The command (e.g., "review", "cancel", "enable-reviews").
        command: String,
        /// The GitHub username who issued the command.
        user: String,
    },

    /// The state machine transitioned between states.
    StateTransition {
        /// The state before the transition.
        from_state: String,
        /// The state after the transition.
        to_state: String,
        /// What triggered the transition (event summary).
        trigger: String,
    },

    /// A batch was submitted to OpenAI.
    BatchSubmitted {
        /// The OpenAI batch ID.
        batch_id: String,
        /// The model used for the review.
        model: String,
    },

    /// A batch completed successfully.
    BatchCompleted {
        /// The OpenAI batch ID.
        batch_id: String,
        /// Whether the review found substantive issues.
        has_issues: bool,
    },

    /// A batch failed.
    BatchFailed {
        /// The OpenAI batch ID.
        batch_id: String,
        /// The failure reason.
        reason: String,
    },

    /// A batch was cancelled.
    BatchCancelled {
        /// The OpenAI batch ID, if one was assigned.
        batch_id: Option<String>,
        /// The cancellation reason.
        reason: String,
    },

    /// A comment was posted to the PR.
    CommentPosted {
        /// The GitHub comment ID.
        comment_id: u64,
    },

    /// A check run was created.
    CheckRunCreated {
        /// The GitHub check run ID.
        check_run_id: u64,
    },
}

/// A logged event for a PR.
///
/// Events are stored in the database and retrieved for timeline display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrEvent {
    /// Database row ID (0 for unsaved events).
    pub id: i64,
    /// Repository owner.
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// Pull request number.
    pub pr_number: u64,
    /// The event type and associated data.
    pub event_type: DashboardEventType,
    /// Unix timestamp when the event was recorded.
    pub recorded_at: i64,
}

/// Summary of a PR for the dashboard list view.
///
/// This contains enough information to display in the PR list without
/// loading all events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSummary {
    /// Repository owner.
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// Pull request number.
    pub pr_number: u64,
    /// Current state machine state name.
    pub current_state: String,
    /// Unix timestamp of the most recent event.
    pub latest_event_at: i64,
    /// Total number of events for this PR.
    pub event_count: usize,
    /// Whether reviews are enabled for this PR.
    pub reviews_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dashboard_event_type_serialization_roundtrip() {
        let events = vec![
            DashboardEventType::WebhookReceived {
                action: "opened".to_string(),
                head_sha: "abc1234".to_string(),
            },
            DashboardEventType::CommandReceived {
                command: "review".to_string(),
                user: "testuser".to_string(),
            },
            DashboardEventType::StateTransition {
                from_state: "Idle".to_string(),
                to_state: "Preparing".to_string(),
                trigger: "PrUpdated".to_string(),
            },
            DashboardEventType::BatchSubmitted {
                batch_id: "batch_123".to_string(),
                model: "gpt-4o".to_string(),
            },
            DashboardEventType::BatchCompleted {
                batch_id: "batch_123".to_string(),
                has_issues: true,
            },
            DashboardEventType::BatchFailed {
                batch_id: "batch_123".to_string(),
                reason: "rate limited".to_string(),
            },
            DashboardEventType::BatchCancelled {
                batch_id: Some("batch_123".to_string()),
                reason: "superseded".to_string(),
            },
            DashboardEventType::BatchCancelled {
                batch_id: None,
                reason: "user requested".to_string(),
            },
            DashboardEventType::CommentPosted { comment_id: 12345 },
            DashboardEventType::CheckRunCreated { check_run_id: 67890 },
        ];

        for event in events {
            let json = serde_json::to_string(&event).expect("serialization should succeed");
            let parsed: DashboardEventType =
                serde_json::from_str(&json).expect("deserialization should succeed");
            assert_eq!(event, parsed, "roundtrip failed for {:?}", event);
        }
    }

    #[test]
    fn test_pr_event_serialization() {
        let event = PrEvent {
            id: 42,
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
            event_type: DashboardEventType::StateTransition {
                from_state: "Idle".to_string(),
                to_state: "Preparing".to_string(),
                trigger: "PrUpdated".to_string(),
            },
            recorded_at: 1704825600,
        };

        let json = serde_json::to_string(&event).expect("serialization should succeed");
        let parsed: PrEvent =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(event, parsed);
    }

    #[test]
    fn test_pr_summary_serialization() {
        let summary = PrSummary {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
            current_state: "BatchPending".to_string(),
            latest_event_at: 1704825600,
            event_count: 5,
            reviews_enabled: true,
        };

        let json = serde_json::to_string(&summary).expect("serialization should succeed");
        let parsed: PrSummary =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(summary, parsed);
    }

    #[test]
    fn test_event_type_json_structure() {
        // Verify the tagged enum produces the expected JSON structure
        let event = DashboardEventType::StateTransition {
            from_state: "Idle".to_string(),
            to_state: "Preparing".to_string(),
            trigger: "PrUpdated".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["type"], "StateTransition");
        assert_eq!(value["data"]["from_state"], "Idle");
        assert_eq!(value["data"]["to_state"], "Preparing");
        assert_eq!(value["data"]["trigger"], "PrUpdated");
    }
}
