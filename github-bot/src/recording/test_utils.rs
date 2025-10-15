use super::types::*;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct MockGenerator;

impl MockGenerator {
    pub fn generate_mocks_from_log<P: AsRef<Path>>(log_path: P) -> Result<TestSuite> {
        let path = log_path.as_ref();

        if path.is_dir() {
            Self::generate_mocks_from_directory(path)
        } else {
            Self::generate_mocks_from_single_file(path)
        }
    }

    pub fn generate_mocks_from_directory<P: AsRef<Path>>(dir_path: P) -> Result<TestSuite> {
        let dir_path = dir_path.as_ref();
        let mut all_events = Vec::new();

        let entries = fs::read_dir(dir_path)
            .with_context(|| format!("Failed to read directory {:?}", dir_path))?;

        let mut file_count = 0;
        for entry in entries {
            let entry = entry.context("Failed to read directory entry")?;
            let path = entry.path();

            if path.is_file() {
                if let Some(extension) = path.extension() {
                    if extension == "jsonl" || extension == "log" {
                        let events = Self::read_events_from_file(&path)?;
                        all_events.extend(events);
                        file_count += 1;
                    }
                } else if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("recordings"))
                {
                    let events = Self::read_events_from_file(&path)?;
                    all_events.extend(events);
                    file_count += 1;
                }
            }
        }

        if file_count == 0 {
            return Err(anyhow!(
                "No recording files found in directory {:?}",
                dir_path
            ));
        }

        if all_events.is_empty() {
            return Err(anyhow!("No events found in any recording files"));
        }

        Self::group_events_into_test_cases(all_events)
    }

    pub fn generate_mocks_from_single_file<P: AsRef<Path>>(file_path: P) -> Result<TestSuite> {
        let events = Self::read_events_from_file(file_path.as_ref())?;

        if events.is_empty() {
            return Err(anyhow!("No events found in log file"));
        }

        Self::group_events_into_test_cases(events)
    }

    fn read_events_from_file<P: AsRef<Path>>(file_path: P) -> Result<Vec<RecordedEvent>> {
        let file = File::open(file_path.as_ref())
            .with_context(|| format!("Failed to open log file {:?}", file_path.as_ref()))?;
        let reader = BufReader::new(file);

        let mut events: Vec<RecordedEvent> = Vec::new();
        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("Failed to read line {}", line_num + 1))?;

            if !line.trim().is_empty() {
                let event: RecordedEvent = serde_json::from_str(&line).map_err(|e| {
                    anyhow!(
                        "Failed to parse line {} in {:?}: {}",
                        line_num + 1,
                        file_path.as_ref(),
                        e
                    )
                })?;
                events.push(event);
            }
        }

        Ok(events)
    }

    fn group_events_into_test_cases(events: Vec<RecordedEvent>) -> Result<TestSuite> {
        let mut test_cases = HashMap::new();

        // Group events by correlation_id to create test scenarios
        for event in events {
            test_cases
                .entry(event.correlation_id.clone())
                .or_insert_with(Vec::new)
                .push(event);
        }

        let scenarios: Vec<TestScenario> = test_cases
            .into_iter()
            .map(|(correlation_id, events)| TestScenario {
                name: format!("scenario_{}", correlation_id),
                events,
            })
            .collect();

        Ok(TestSuite { scenarios })
    }

    pub fn generate_mock_server_config(test_suite: &TestSuite) -> Result<String> {
        let mut wiremock_stubs = Vec::new();

        for scenario in &test_suite.scenarios {
            for event in &scenario.events {
                if let Direction::Request = event.direction {
                    if let Some(response_event) =
                        Self::find_matching_response(&scenario.events, event)
                    {
                        let stub = Self::create_wiremock_stub(event, response_event)?;
                        wiremock_stubs.push(stub);
                    }
                }
            }
        }

        let config = serde_json::json!({
            "mappings": wiremock_stubs
        });

        serde_json::to_string_pretty(&config).context("Failed to serialize mock config")
    }

    fn find_matching_response<'a>(
        events: &'a [RecordedEvent],
        request: &RecordedEvent,
    ) -> Option<&'a RecordedEvent> {
        events.iter().find(|e| {
            e.correlation_id == request.correlation_id
                && matches!(e.direction, Direction::Response)
                && e.event_type == request.event_type
        })
    }

    fn create_wiremock_stub(
        request: &RecordedEvent,
        response: &RecordedEvent,
    ) -> Result<serde_json::Value> {
        // Extract request data
        let req_data = request
            .data
            .as_object()
            .ok_or_else(|| anyhow!("Invalid request data format"))?;

        let method = req_data
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET");

        let url = req_data
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing URL in request data"))?;

        // Extract response data
        let resp_data = response
            .data
            .as_object()
            .ok_or_else(|| anyhow!("Invalid response data format"))?;

        let status = resp_data
            .get("status_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(200);

        Ok(serde_json::json!({
            "request": {
                "method": method,
                "urlPattern": Self::url_to_pattern(url)?
            },
            "response": {
                "status": status,
                "headers": resp_data.get("headers").unwrap_or(&serde_json::Value::Object(serde_json::Map::new())),
                "body": format!("{{\"recorded_at\": \"{}\", \"correlation_id\": \"{}\"}}", response.timestamp, response.correlation_id)
            }
        }))
    }

    fn url_to_pattern(url: &str) -> Result<String> {
        let parsed =
            url::Url::parse(url).with_context(|| format!("Failed to parse URL {}", url))?;

        // Convert URL to a pattern that matches similar requests
        let mut pattern = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));

        if let Some(port) = parsed.port() {
            pattern.push_str(&format!(":{}", port));
        }

        pattern.push_str(parsed.path());

        // Add query parameters as optional pattern
        if !parsed.query().unwrap_or("").is_empty() {
            pattern.push_str(".*");
        }

        Ok(pattern)
    }
}

#[derive(Debug)]
pub struct TestSuite {
    pub scenarios: Vec<TestScenario>,
}

#[derive(Debug)]
pub struct TestScenario {
    pub name: String,
    pub events: Vec<RecordedEvent>,
}
