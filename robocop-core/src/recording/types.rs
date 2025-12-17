use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordedEvent {
    pub timestamp: String,      // ISO 8601 timestamp
    pub correlation_id: String, // Unique ID to group related events
    pub event_type: EventType,
    pub direction: Direction,
    pub operation: String,       // e.g., "webhook", "get_diff", "create_batch"
    pub data: serde_json::Value, // Sanitized request/response data
    pub metadata: HashMap<String, String>, // PR number, repo, etc.
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum EventType {
    WebhookReceived,
    GitHubApiCall,
    OpenAiApiCall,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Direction {
    Request,
    Response,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ServiceType {
    GitHub,
    OpenAi,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HttpEvent {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>, // Sanitized headers
    pub body: Option<serde_json::Value>,
    pub status_code: Option<u16>, // Only for responses
}

// Correlation ID type for better type safety
#[derive(Clone, Debug)]
pub struct CorrelationId(pub String);

// Header name for correlation ID propagation
pub const CORRELATION_ID_HEADER: &str = "X-Correlation-ID";
