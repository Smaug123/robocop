use super::sanitizer::Sanitizer;
use super::types::{CorrelationId, CORRELATION_ID_HEADER};
use super::{Direction, EventType, RecordedEvent, RecordingLogger, ServiceType};
use axum::http;
use reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next, Result as MiddlewareResult};
use std::collections::HashMap;
use uuid::Uuid;

pub struct RecordingMiddleware {
    logger: RecordingLogger,
    service_type: ServiceType,
}

impl RecordingMiddleware {
    pub fn new(logger: RecordingLogger, service_type: ServiceType) -> Self {
        Self {
            logger,
            service_type,
        }
    }
}

#[async_trait::async_trait]
impl Middleware for RecordingMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> MiddlewareResult<Response> {
        // Get correlation ID from existing header or create new one
        let correlation_id = if let Some(existing_header) = req.headers().get(CORRELATION_ID_HEADER)
        {
            // Use existing correlation ID from header if present
            match existing_header.to_str() {
                Ok(header_value) => header_value.to_string(),
                Err(_) => {
                    // If header exists but is invalid, generate new ID
                    Uuid::new_v4().to_string()
                }
            }
        } else {
            // Check extensions as fallback, then generate new ID
            extensions
                .get::<CorrelationId>()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string())
        };

        // Add correlation ID header to outgoing request only if not already present
        if !req.headers().contains_key(CORRELATION_ID_HEADER) {
            req.headers_mut().insert(
                CORRELATION_ID_HEADER,
                correlation_id
                    .parse()
                    .expect("correlation ID should be valid header value"),
            );
        }

        // Clone request data for recording (before consumption)
        let request_data = self.extract_request_data(&req).await;

        // Record request
        self.record_request(&request_data, &correlation_id);

        // Execute request
        let response = next.run(req, extensions).await;

        // Record response or error
        match &response {
            Ok(resp) => {
                let response_data = self.extract_response_data(resp).await;
                self.record_response(&response_data, &correlation_id);
            }
            Err(err) => {
                self.record_error(err, &correlation_id);
            }
        }

        response
    }
}

impl RecordingMiddleware {
    async fn extract_request_data(&self, request: &Request) -> RequestData {
        let mut headers = HashMap::new();
        for (name, value) in request.headers() {
            if let Ok(value_str) = value.to_str() {
                headers.insert(name.to_string(), value_str.to_string());
            }
        }

        // Extract body size for logging (actual body content handled by middleware layer)
        let body_info = if let Some(body) = request.body() {
            match body.as_bytes() {
                Some(bytes) => {
                    let size = bytes.len();
                    if size > 10_000 {
                        format!("[LARGE_BODY_{}b]", size)
                    } else if let Ok(text) = std::str::from_utf8(bytes) {
                        text.to_string()
                    } else {
                        format!("[BINARY_BODY_{}b]", size)
                    }
                }
                None => "[STREAM_BODY]".to_string(),
            }
        } else {
            "[NO_BODY]".to_string()
        };

        RequestData {
            method: request.method().to_string(),
            url: request.url().to_string(),
            headers: Sanitizer::sanitize_headers(&headers),
            body: body_info,
        }
    }

    async fn extract_response_data(&self, response: &Response) -> ResponseData {
        let mut headers = HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(value_str) = value.to_str() {
                headers.insert(name.to_string(), value_str.to_string());
            }
        }

        ResponseData {
            status_code: response.status().as_u16(),
            headers: Sanitizer::sanitize_headers(&headers),
            body_size: response.content_length().unwrap_or(0),
        }
    }

    fn record_request(&self, request_data: &RequestData, correlation_id: &str) {
        let event = RecordedEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            correlation_id: correlation_id.to_string(),
            event_type: match self.service_type {
                ServiceType::GitHub => EventType::GitHubApiCall,
                ServiceType::OpenAi => EventType::OpenAiApiCall,
            },
            direction: Direction::Request,
            operation: format!(
                "{} {}",
                request_data.method,
                extract_path(&request_data.url)
            ),
            data: serde_json::to_value(request_data).unwrap_or(serde_json::Value::Null),
            metadata: HashMap::new(),
        };

        self.logger.record(event);
    }

    fn record_response(&self, response_data: &ResponseData, correlation_id: &str) {
        let event = RecordedEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            correlation_id: correlation_id.to_string(),
            event_type: match self.service_type {
                ServiceType::GitHub => EventType::GitHubApiCall,
                ServiceType::OpenAi => EventType::OpenAiApiCall,
            },
            direction: Direction::Response,
            operation: format!("response_{}", response_data.status_code),
            data: serde_json::to_value(response_data).unwrap_or(serde_json::Value::Null),
            metadata: HashMap::new(),
        };

        self.logger.record(event);
    }

    fn record_error(&self, error: &reqwest_middleware::Error, correlation_id: &str) {
        let event = RecordedEvent {
            timestamp: chrono::Utc::now().to_rfc3339(),
            correlation_id: correlation_id.to_string(),
            event_type: match self.service_type {
                ServiceType::GitHub => EventType::GitHubApiCall,
                ServiceType::OpenAi => EventType::OpenAiApiCall,
            },
            direction: Direction::Response,
            operation: "error".to_string(),
            data: serde_json::json!({
                "error": error.to_string(),
                "error_type": format!("{:?}", error)
            }),
            metadata: HashMap::new(),
        };

        self.logger.record(event);
    }
}

#[derive(Debug, serde::Serialize)]
struct RequestData {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Debug, serde::Serialize)]
struct ResponseData {
    status_code: u16,
    headers: HashMap<String, String>,
    body_size: u64,
}

fn extract_path(url: &str) -> String {
    url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string())
}
