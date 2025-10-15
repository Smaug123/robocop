use github_bot::recording::test_utils::{MockGenerator, TestSuite};
use github_bot::recording::types::{Direction, EventType, RecordedEvent};
use insta::{assert_debug_snapshot, with_settings};
use serde_json::json;
use std::fs;

const RECORDINGS_PATHS: &[&str] = &["tests/recordings/sample1.jsonl"];

fn get_available_recording_paths() -> Vec<String> {
    let mut paths = Vec::new();

    for &base_path in RECORDINGS_PATHS {
        if std::path::Path::new(base_path).exists() {
            paths.push(base_path.to_string());
        }
    }

    // Also look for any directories that start with "recordings"
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("recordings") && !paths.contains(&name.to_string()) {
                        paths.push(name.to_string());
                    }
                }
            }
        }
    }

    paths
}

fn test_webhook_events_for_path(recordings_path: &str) {
    let test_suite = MockGenerator::generate_mocks_from_log(recordings_path)
        .expect("Failed to generate test suite from recordings");

    // Create snapshot for webhook events only
    let webhook_events: Vec<&RecordedEvent> = test_suite
        .scenarios
        .iter()
        .flat_map(|scenario| &scenario.events)
        .filter(|event| matches!(event.event_type, EventType::WebhookReceived))
        .collect();

    let snapshot_data = create_webhook_snapshot(&webhook_events, recordings_path);

    // Sanitize the path for snapshot name
    let snapshot_name = format!(
        "webhook_events_{}",
        recordings_path.replace(['/', '-'], "_")
    );

    with_settings!({
        description => format!("Webhook events from recordings: {}", recordings_path),
        omit_expression => true
    }, {
        assert_debug_snapshot!(snapshot_name, snapshot_data);
    });
}

#[tokio::test]
async fn test_webhook_events_snapshot() {
    let recording_paths = get_available_recording_paths();
    for path in recording_paths {
        test_webhook_events_for_path(&path);
    }
}

fn test_github_api_calls_for_path(recordings_path: &str) {
    let test_suite = MockGenerator::generate_mocks_from_log(recordings_path)
        .expect("Failed to generate test suite from recordings");

    let github_events: Vec<&RecordedEvent> = test_suite
        .scenarios
        .iter()
        .flat_map(|scenario| &scenario.events)
        .filter(|event| matches!(event.event_type, EventType::GitHubApiCall))
        .collect();

    let snapshot_data = create_api_calls_snapshot(&github_events, "GitHub", recordings_path);

    let snapshot_name = format!(
        "github_api_calls_{}",
        recordings_path.replace(['/', '-'], "_")
    );

    with_settings!({
        description => format!("GitHub API calls from recordings: {}", recordings_path),
        omit_expression => true
    }, {
        assert_debug_snapshot!(snapshot_name, snapshot_data);
    });
}

#[tokio::test]
async fn test_github_api_calls_snapshot() {
    let recording_paths = get_available_recording_paths();
    for path in recording_paths {
        test_github_api_calls_for_path(&path);
    }
}

fn test_openai_api_calls_for_path(recordings_path: &str) {
    let test_suite = MockGenerator::generate_mocks_from_log(recordings_path)
        .expect("Failed to generate test suite from recordings");

    let openai_events: Vec<&RecordedEvent> = test_suite
        .scenarios
        .iter()
        .flat_map(|scenario| &scenario.events)
        .filter(|event| matches!(event.event_type, EventType::OpenAiApiCall))
        .collect();

    let snapshot_data = create_api_calls_snapshot(&openai_events, "OpenAI", recordings_path);

    let snapshot_name = format!(
        "openai_api_calls_{}",
        recordings_path.replace(['/', '-'], "_")
    );

    with_settings!({
        description => format!("OpenAI API calls from recordings: {}", recordings_path),
        omit_expression => true
    }, {
        assert_debug_snapshot!(snapshot_name, snapshot_data);
    });
}

#[tokio::test]
async fn test_openai_api_calls_snapshot() {
    let recording_paths = get_available_recording_paths();
    for path in recording_paths {
        test_openai_api_calls_for_path(&path);
    }
}

fn test_complete_scenarios_for_path(recordings_path: &str) {
    let test_suite = MockGenerator::generate_mocks_from_log(recordings_path)
        .expect("Failed to generate test suite from recordings");

    // Create a snapshot of complete scenarios grouped by correlation_id
    let snapshot_data = create_scenarios_snapshot(&test_suite, recordings_path);

    let snapshot_name = format!(
        "complete_scenarios_{}",
        recordings_path.replace(['/', '-'], "_")
    );

    with_settings!({
        description => format!("Complete scenarios from recordings: {}", recordings_path),
        omit_expression => true
    }, {
        assert_debug_snapshot!(snapshot_name, snapshot_data);
    });
}

#[tokio::test]
async fn test_complete_scenarios_snapshot() {
    let recording_paths = get_available_recording_paths();
    for path in recording_paths {
        test_complete_scenarios_for_path(&path);
    }
}

fn test_request_response_pairs_for_path(recordings_path: &str) {
    let test_suite = MockGenerator::generate_mocks_from_log(recordings_path)
        .expect("Failed to generate test suite from recordings");

    // Extract request/response pairs for validation
    let pairs = extract_request_response_pairs(&test_suite);

    let snapshot_name = format!(
        "request_response_pairs_{}",
        recordings_path.replace(['/', '-'], "_")
    );

    with_settings!({
        description => format!("Request/response pairs from recordings: {}", recordings_path),
        omit_expression => true
    }, {
        assert_debug_snapshot!(snapshot_name, json!(pairs));
    });
}

#[tokio::test]
async fn test_request_response_pairs_snapshot() {
    let recording_paths = get_available_recording_paths();
    for path in recording_paths {
        test_request_response_pairs_for_path(&path);
    }
}

fn create_webhook_snapshot(events: &[&RecordedEvent], recordings_path: &str) -> serde_json::Value {
    let mut webhook_data: Vec<serde_json::Value> = events
        .iter()
        .map(|event| {
            json!({
                "correlation_id": event.correlation_id,
                "timestamp": event.timestamp,
                "operation": event.operation,
                "event_summary": extract_webhook_summary(&event.data),
                "sanitized_data": sanitize_webhook_data(&event.data)
            })
        })
        .collect();

    // Sort by timestamp for deterministic output
    webhook_data.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .unwrap()
            .cmp(b["timestamp"].as_str().unwrap())
    });

    json!({
        "source": recordings_path,
        "total_webhook_events": webhook_data.len(),
        "events": webhook_data
    })
}

fn create_api_calls_snapshot(
    events: &[&RecordedEvent],
    service_name: &str,
    recordings_path: &str,
) -> serde_json::Value {
    let mut requests = Vec::new();
    let mut responses = Vec::new();

    for event in events {
        match event.direction {
            Direction::Request => {
                requests.push(json!({
                    "correlation_id": event.correlation_id,
                    "timestamp": event.timestamp,
                    "operation": event.operation,
                    "request_summary": extract_api_request_summary(&event.data),
                    "sanitized_data": sanitize_api_data(&event.data)
                }));
            }
            Direction::Response => {
                responses.push(json!({
                    "correlation_id": event.correlation_id,
                    "timestamp": event.timestamp,
                    "operation": event.operation,
                    "response_summary": extract_api_response_summary(&event.data),
                    "sanitized_data": sanitize_api_data(&event.data)
                }));
            }
        }
    }

    // Sort by timestamp for deterministic output
    requests.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .unwrap()
            .cmp(b["timestamp"].as_str().unwrap())
    });
    responses.sort_by(|a, b| {
        a["timestamp"]
            .as_str()
            .unwrap()
            .cmp(b["timestamp"].as_str().unwrap())
    });

    json!({
        "source": recordings_path,
        "service": service_name,
        "total_requests": requests.len(),
        "total_responses": responses.len(),
        "requests": requests,
        "responses": responses
    })
}

fn create_scenarios_snapshot(test_suite: &TestSuite, recordings_path: &str) -> serde_json::Value {
    let mut scenarios: Vec<serde_json::Value> = test_suite
        .scenarios
        .iter()
        .map(|scenario| {
            let mut events: Vec<serde_json::Value> = scenario
                .events
                .iter()
                .map(|event| {
                    json!({
                        "timestamp": event.timestamp,
                        "event_type": format!("{:?}", event.event_type),
                        "direction": format!("{:?}", event.direction),
                        "operation": event.operation,
                        "summary": create_event_summary(event),
                        "sanitized_data": sanitize_event_data(event)
                    })
                })
                .collect();

            // Sort events by timestamp
            events.sort_by(|a, b| {
                a["timestamp"].as_str().unwrap().cmp(b["timestamp"].as_str().unwrap())
            });

            json!({
                "scenario_name": scenario.name,
                "correlation_id": scenario.events.first().map(|e| &e.correlation_id),
                "event_count": scenario.events.len(),
                "event_types": scenario.events.iter().map(|e| format!("{:?}", e.event_type)).collect::<Vec<_>>(),
                "events": events
            })
        })
        .collect();

    // Sort scenarios by correlation_id for deterministic output
    scenarios.sort_by(|a, b| {
        a["correlation_id"]
            .as_str()
            .unwrap()
            .cmp(b["correlation_id"].as_str().unwrap())
    });

    json!({
        "source": recordings_path,
        "total_scenarios": scenarios.len(),
        "scenarios": scenarios
    })
}

fn extract_request_response_pairs(test_suite: &TestSuite) -> Vec<serde_json::Value> {
    let mut pairs = Vec::new();

    for scenario in &test_suite.scenarios {
        let requests: Vec<&RecordedEvent> = scenario
            .events
            .iter()
            .filter(|e| matches!(e.direction, Direction::Request))
            .collect();

        for request in requests {
            if let Some(response) = scenario.events.iter().find(|e| {
                matches!(e.direction, Direction::Response)
                    && e.event_type == request.event_type
                    && e.correlation_id == request.correlation_id
            }) {
                pairs.push(json!({
                    "correlation_id": request.correlation_id,
                    "operation": request.operation,
                    "event_type": format!("{:?}", request.event_type),
                    "request": {
                        "timestamp": request.timestamp,
                        "summary": create_event_summary(request),
                        "sanitized_data": sanitize_event_data(request)
                    },
                    "response": {
                        "timestamp": response.timestamp,
                        "summary": create_event_summary(response),
                        "sanitized_data": sanitize_event_data(response)
                    }
                }));
            }
        }
    }

    // Sort pairs by request timestamp for deterministic output
    pairs.sort_by(|a, b| {
        a["request"]["timestamp"]
            .as_str()
            .unwrap()
            .cmp(b["request"]["timestamp"].as_str().unwrap())
    });

    pairs
}

fn extract_webhook_summary(data: &serde_json::Value) -> serde_json::Value {
    if let Some(body) = data.get("body") {
        json!({
            "action": body.get("action"),
            "ref": body.get("ref"),
            "repository_name": body.get("repository").and_then(|r| r.get("name")),
            "sender": body.get("sender").and_then(|s| s.get("login")),
            "commits_count": body.get("commits").and_then(|c| c.as_array()).map(|a| a.len())
        })
    } else {
        json!({})
    }
}

fn extract_api_request_summary(data: &serde_json::Value) -> serde_json::Value {
    json!({
        "method": data.get("method"),
        "url_pattern": extract_url_pattern(data.get("url")),
        "has_body": data.get("body").is_some()
    })
}

fn extract_api_response_summary(data: &serde_json::Value) -> serde_json::Value {
    json!({
        "status_code": data.get("status_code"),
        "has_body": data.get("body").is_some(),
        "content_type": data.get("headers").and_then(|h| h.get("content-type"))
    })
}

fn extract_url_pattern(url: Option<&serde_json::Value>) -> Option<String> {
    url.and_then(|u| u.as_str()).map(|url_str| {
        if let Ok(parsed) = url::Url::parse(url_str) {
            format!("{}{}", parsed.host_str().unwrap_or(""), parsed.path())
        } else {
            url_str.to_string()
        }
    })
}

fn create_event_summary(event: &RecordedEvent) -> serde_json::Value {
    match event.event_type {
        EventType::WebhookReceived => extract_webhook_summary(&event.data),
        EventType::GitHubApiCall | EventType::OpenAiApiCall => match event.direction {
            Direction::Request => extract_api_request_summary(&event.data),
            Direction::Response => extract_api_response_summary(&event.data),
        },
    }
}

fn sanitize_webhook_data(data: &serde_json::Value) -> serde_json::Value {
    let mut sanitized = data.clone();

    // Remove/redact sensitive information in headers
    if let Some(headers) = sanitized.get_mut("headers") {
        if let Some(headers_obj) = headers.as_object_mut() {
            // Redact sensitive headers
            for sensitive_header in ["x-hub-signature-256", "authorization", "x-github-token"] {
                if headers_obj.contains_key(sensitive_header) {
                    headers_obj.insert(sensitive_header.to_string(), json!("[REDACTED]"));
                }
            }
        }
    }

    // Keep structure but remove large nested objects for clarity
    if let Some(body) = sanitized.get_mut("body") {
        if let Some(repo) = body.get_mut("repository") {
            if let Some(repo_obj) = repo.as_object_mut() {
                // Keep only essential repo fields
                let essential_fields = ["id", "name", "full_name", "private", "default_branch"];
                repo_obj.retain(|k, _| essential_fields.contains(&k.as_str()));
            }
        }
    }

    sanitized
}

fn sanitize_api_data(data: &serde_json::Value) -> serde_json::Value {
    let mut sanitized = data.clone();

    // Redact authorization headers
    if let Some(headers) = sanitized.get_mut("headers") {
        if let Some(headers_obj) = headers.as_object_mut() {
            for auth_header in ["authorization", "x-github-token", "openai-api-key"] {
                if headers_obj.contains_key(auth_header) {
                    headers_obj.insert(auth_header.to_string(), json!("[REDACTED]"));
                }
            }
        }
    }

    // For large response bodies, summarize instead of including full content
    if let Some(body) = sanitized.get_mut("body") {
        if let Some(body_str) = body.as_str() {
            if body_str.len() > 1000 {
                let length = body_str.len();
                sanitized.as_object_mut().unwrap().insert(
                    "body".to_string(),
                    json!(format!("[LARGE_BODY: {} chars]", length)),
                );
            }
        }
    }

    sanitized
}

fn sanitize_event_data(event: &RecordedEvent) -> serde_json::Value {
    match event.event_type {
        EventType::WebhookReceived => sanitize_webhook_data(&event.data),
        EventType::GitHubApiCall | EventType::OpenAiApiCall => sanitize_api_data(&event.data),
    }
}
