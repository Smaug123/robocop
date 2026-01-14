use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use robocop_core::{
    create_user_prompt, get_system_prompt, GitData, OpenAIClient, ReviewMetadata, DEFAULT_MODEL,
};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Robocop: AI-powered code review tool
#[derive(Parser, Debug)]
#[command(name = "robocop")]
#[command(about = "AI-powered code review tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a code review on the current git branch
    Review(ReviewArgs),
    /// List available OpenAI models
    ListModels(ListModelsArgs),
    /// Poll for a response by ID (use after stream interruption)
    Poll(PollArgs),
    /// Cancel an in-progress background response
    Cancel(CancelArgs),
}

#[derive(Parser, Debug)]
struct ReviewArgs {
    /// Default branch name to compare against
    #[arg(long, default_value = "main")]
    default_branch: String,

    /// If set, do not make any changes, just print what would be done
    #[arg(long)]
    dry_run: bool,

    /// OpenAI API key (if not provided, will use OPENAI_API_KEY environment variable)
    #[arg(long)]
    api_key: Option<String>,

    /// Additional context to add to the user prompt
    #[arg(long, default_value = "")]
    additional_prompt: String,

    /// Additional files to include as context
    #[arg(long, num_args = 1..)]
    include_files: Vec<String>,

    /// Reasoning effort level
    #[arg(long, default_value = "high", value_parser = ["none", "minimal", "low", "medium", "high", "xhigh"])]
    reasoning_effort: String,

    /// Use OpenAI batch processing API
    #[arg(long)]
    batch: bool,

    /// OpenAI model to use for the review
    #[arg(long, default_value = DEFAULT_MODEL)]
    model: String,
}

#[derive(Parser, Debug)]
struct ListModelsArgs {
    /// OpenAI API key (if not provided, will use OPENAI_API_KEY environment variable)
    #[arg(long)]
    api_key: Option<String>,
}

#[derive(Parser, Debug)]
struct PollArgs {
    /// Response ID to poll (e.g., resp_67c9fdce...)
    response_id: String,

    /// Sequence number to resume from (for partial responses)
    #[arg(long)]
    sequence: Option<u64>,

    /// OpenAI API key (if not provided, will use OPENAI_API_KEY environment variable)
    #[arg(long)]
    api_key: Option<String>,

    /// Print full event data for each SSE event
    #[arg(long)]
    debug: bool,
}

#[derive(Parser, Debug)]
struct CancelArgs {
    /// Response ID to cancel (e.g., resp_67c9fdce...)
    response_id: String,

    /// OpenAI API key (if not provided, will use OPENAI_API_KEY environment variable)
    #[arg(long)]
    api_key: Option<String>,
}

/// Get git diff of current working directory against the merge-base
fn get_git_diff(default_branch: &str) -> Result<GitData> {
    // Get HEAD hash
    let head_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("Failed to execute git rev-parse HEAD")?;

    if !head_output.status.success() {
        let stderr = String::from_utf8_lossy(&head_output.stderr);
        return Err(anyhow!("git rev-parse HEAD failed: {}", stderr));
    }

    let head_hash = String::from_utf8(head_output.stdout)
        .context("Failed to parse HEAD hash as UTF-8")?
        .trim()
        .to_string();

    // Get merge base
    let merge_base_output = Command::new("git")
        .args(["merge-base", "HEAD", default_branch])
        .output()
        .context("Failed to execute git merge-base")?;

    if !merge_base_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_base_output.stderr);
        return Err(anyhow!(
            "git merge-base HEAD {} failed: {}",
            default_branch,
            stderr
        ));
    }

    let merge_base = String::from_utf8(merge_base_output.stdout)
        .context("Failed to parse merge base as UTF-8")?
        .trim()
        .to_string();

    // Get branch name
    let branch_output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .context("Failed to execute git branch --show-current")?;
    let branch_name = String::from_utf8(branch_output.stdout)
        .context("Failed to parse branch name as UTF-8")?
        .trim()
        .to_string();
    let branch_name = if branch_name.is_empty() {
        None
    } else {
        Some(branch_name)
    };

    // Get diff against merge-base
    let diff_output = Command::new("git")
        .args([
            "diff",
            "--no-ext-diff",
            "--unified=5",
            "--no-color",
            &merge_base,
        ])
        .output()
        .context("Failed to execute git diff")?;

    if !diff_output.status.success() {
        let stderr = String::from_utf8_lossy(&diff_output.stderr);
        return Err(anyhow!("git diff failed: {}", stderr));
    }

    let diff = String::from_utf8(diff_output.stdout).context("Failed to parse diff as UTF-8")?;

    // Get list of changed files
    let files_output = Command::new("git")
        .args(["diff", "--no-ext-diff", "--name-only", &merge_base])
        .output()
        .context("Failed to execute git diff --name-only")?;

    if !files_output.status.success() {
        let stderr = String::from_utf8_lossy(&files_output.stderr);
        return Err(anyhow!("git diff --name-only failed: {}", stderr));
    }

    let files_changed = String::from_utf8(files_output.stdout)
        .context("Failed to parse changed files as UTF-8")?
        .lines()
        .map(|s| s.to_string())
        .collect();

    // Get repo name from toplevel directory
    let repo_path = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("Failed to execute git rev-parse --show-toplevel")?
            .stdout,
    )
    .context("Failed to parse toplevel path as UTF-8")?
    .trim()
    .to_string();
    let repo_name = Path::new(&repo_path)
        .file_name()
        .context("Failed to extract repo name from path")?
        .to_str()
        .context("Repo name is not valid UTF-8")?
        .to_string();

    // Get remote URL if available
    let remote_url = if let Some(ref branch) = branch_name {
        // Try to get remote name for this branch
        let remote_result = Command::new("git")
            .args(["config", "--get", &format!("branch.{}.remote", branch)])
            .stderr(Stdio::null())
            .output();

        if let Ok(remote_output) = remote_result {
            if remote_output.status.success() {
                if let Ok(remote_name) = String::from_utf8(remote_output.stdout) {
                    let remote_name = remote_name.trim().to_string();

                    if !remote_name.is_empty() {
                        // Get URL for this remote
                        let url_result = Command::new("git")
                            .args(["remote", "get-url", &remote_name])
                            .stderr(Stdio::null())
                            .output();

                        if let Ok(url_output) = url_result {
                            if url_output.status.success() {
                                if let Ok(url) = String::from_utf8(url_output.stdout) {
                                    Some(url.trim().to_string())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(GitData::new(
        diff,
        files_changed,
        head_hash,
        merge_base,
        branch_name,
        repo_name,
        remote_url,
    ))
}

/// Read a file and return its contents
/// Returns None if file doesn't exist or can't be decoded as UTF-8
fn read_file(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Response from OpenAI models list API
#[derive(serde::Deserialize, Debug)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(serde::Deserialize, Debug)]
struct ModelInfo {
    id: String,
}

/// SSE stream processor that handles UTF-8 chunk boundaries correctly
struct SseProcessor {
    buffer: Vec<u8>,
}

impl SseProcessor {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Add a chunk of bytes to the buffer
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Try to extract the next complete SSE event from the buffer.
    /// Returns None if no complete event is available yet.
    fn next_event(&mut self) -> Option<Result<(String, String)>> {
        // Loop to skip comment/empty events without recursion
        loop {
            // Look for event boundary: \n\n or \r\n\r\n
            let boundary = self.find_event_boundary()?;

            // Extract the event bytes
            let event_bytes: Vec<u8> = self.buffer.drain(..boundary.end).collect();

            // Decode as UTF-8
            let event_str = match std::str::from_utf8(&event_bytes[..boundary.start]) {
                Ok(s) => s.to_string(),
                Err(e) => return Some(Err(anyhow!("Invalid UTF-8 in SSE event: {}", e))),
            };

            // Parse the event - if parsing fails (no data), continue to next event
            if let Some(parsed) = Self::parse_event(&event_str) {
                return Some(Ok(parsed));
            }
            // Otherwise loop to try next event (e.g., skip comments, keepalives)
        }
    }

    /// Find the next event boundary, returning the range to consume
    /// (start = end of event content, end = end including delimiter)
    fn find_event_boundary(&self) -> Option<std::ops::Range<usize>> {
        // Check for \r\n\r\n first (4 bytes)
        for i in 0..self.buffer.len().saturating_sub(3) {
            if &self.buffer[i..i + 4] == b"\r\n\r\n" {
                return Some(i..i + 4);
            }
        }
        // Check for \n\n (2 bytes)
        for i in 0..self.buffer.len().saturating_sub(1) {
            if &self.buffer[i..i + 2] == b"\n\n" {
                return Some(i..i + 2);
            }
        }
        None
    }

    /// Parse an SSE event string into event type and data.
    /// Per SSE spec: space after colon is optional, multiple data lines are concatenated with newlines.
    /// If no event: line is present, defaults to "message" per SSE spec.
    fn parse_event(event_str: &str) -> Option<(String, String)> {
        let mut event_type = None;
        let mut data_lines: Vec<String> = Vec::new();

        for line in event_str.lines() {
            // Handle potential \r at end of line (for mixed line endings)
            let line = line.trim_end_matches('\r');

            // Parse "event:" - space after colon is optional per SSE spec
            if let Some(rest) = line.strip_prefix("event:") {
                // Skip optional leading space
                let value = rest.strip_prefix(' ').unwrap_or(rest);
                event_type = Some(value.to_string());
            }
            // Parse "data:" - space after colon is optional, multiple lines concatenated
            else if let Some(rest) = line.strip_prefix("data:") {
                let value = rest.strip_prefix(' ').unwrap_or(rest);
                data_lines.push(value.to_string());
            }
            // SSE also supports "id:" and "retry:" but we don't need them
        }

        // Must have at least one data line to be a valid event
        if data_lines.is_empty() {
            None
        } else {
            // Concatenate multiple data lines with newlines per SSE spec
            let data = data_lines.join("\n");
            // Default event type is "message" per SSE spec
            let event_type = event_type.unwrap_or_else(|| "message".to_string());
            Some((event_type, data))
        }
    }

    /// Check if there's any remaining data in the buffer
    fn has_remaining(&self) -> bool {
        // Check if there's non-whitespace content remaining
        self.buffer.iter().any(|&b| !b.is_ascii_whitespace())
    }

    /// Get remaining buffer content as string (for error reporting)
    fn remaining_as_string(&self) -> String {
        String::from_utf8_lossy(&self.buffer).to_string()
    }
}

/// Extract response ID from a response.created event data
fn extract_response_id(data: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    parsed
        .get("response")?
        .get("id")?
        .as_str()
        .map(|s| s.to_string())
}

/// Extract error information from a response.failed or error event
fn extract_error_info(data: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
        // Try response.failed format
        if let Some(error) = parsed.get("response").and_then(|r| r.get("error")) {
            if let Ok(formatted) = serde_json::to_string_pretty(error) {
                return formatted;
            }
        }
        // Try direct error format
        if let Some(error) = parsed.get("error") {
            if let Ok(formatted) = serde_json::to_string_pretty(error) {
                return formatted;
            }
        }
        // Try message field
        if let Some(msg) = parsed.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    data.to_string()
}

/// Extract the final text from a response.completed event data
fn extract_final_text(data: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(data).context("Failed to parse response.completed data")?;

    let response = parsed
        .get("response")
        .context("No response field in response.completed")?;

    let output = response
        .get("output")
        .context("No output field in response")?
        .as_array()
        .context("output is not an array")?;

    let message = output
        .iter()
        .find(|o| o.get("type").and_then(|t| t.as_str()) == Some("message"))
        .context("No message output found")?;

    let content = message
        .get("content")
        .context("No content in message")?
        .as_array()
        .context("content is not an array")?;

    let text_content = content
        .iter()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .context("No output_text content found")?;

    text_content
        .get("text")
        .context("No text field in output_text")?
        .as_str()
        .context("text is not a string")
        .map(|s| s.to_string())
}

/// State tracked during SSE streaming
struct StreamState {
    response_id: Option<String>,
    last_sequence: Option<u64>,
}

/// Result of processing an SSE stream
enum StreamResult {
    /// Stream completed successfully with final text
    Completed(String),
    /// Stream failed with an API error (not retryable)
    Failed(String),
    /// Stream was interrupted (retryable if we have response_id)
    Interrupted { error: anyhow::Error },
}

/// Process an SSE stream, updating state and returning the result
async fn process_sse_stream(response: reqwest::Response, state: &mut StreamState) -> StreamResult {
    let mut stream = response.bytes_stream();
    let mut sse = SseProcessor::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                return StreamResult::Interrupted {
                    error: anyhow::Error::from(e).context("Failed to read chunk from stream"),
                };
            }
        };
        sse.push(&chunk);

        // Process all complete events in the buffer
        while let Some(event_result) = sse.next_event() {
            let (event_type, data) = match event_result {
                Ok(e) => e,
                Err(e) => {
                    return StreamResult::Interrupted {
                        error: e.context("Failed to parse SSE event"),
                    };
                }
            };

            // Extract sequence number but don't commit until after successful processing.
            // This ensures we can retry an event if processing fails.
            let event_sequence = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|parsed| parsed.get("sequence_number").and_then(|s| s.as_u64()));

            match event_type.as_str() {
                "response.created" => {
                    if let Some(id) = extract_response_id(&data) {
                        state.response_id = Some(id.clone());
                        eprintln!("Response ID: {}", id);
                    }
                    if let Some(seq) = event_sequence {
                        state.last_sequence = Some(seq);
                    }
                }
                "response.completed" => match extract_final_text(&data) {
                    Ok(text) => {
                        // Return immediately - we have the final result, no need to
                        // continue reading the stream and risk spurious errors.
                        return StreamResult::Completed(text);
                    }
                    Err(e) => {
                        // Don't update sequence - we want to retry this event
                        return StreamResult::Interrupted {
                            error: e.context("Failed to extract final text"),
                        };
                    }
                },
                "response.failed" => {
                    let error_info = extract_error_info(&data);
                    return StreamResult::Failed(error_info);
                }
                "error" => {
                    let error_info = extract_error_info(&data);
                    return StreamResult::Failed(error_info);
                }
                _ => {
                    if let Some(seq) = event_sequence {
                        state.last_sequence = Some(seq);
                    }
                }
            }
        }
    }

    // Check for incomplete data in buffer
    if sse.has_remaining() {
        let remaining = sse.remaining_as_string();
        eprintln!(
            "Warning: stream ended with unparsed data: {}",
            remaining.chars().take(100).collect::<String>()
        );
    }

    // If we reach here, the stream ended without a response.completed event
    StreamResult::Interrupted {
        error: anyhow!("Stream ended without response.completed event"),
    }
}

/// Poll a stored response by ID, optionally resuming from a sequence number
async fn poll_response(
    client: &reqwest::Client,
    api_key: &str,
    response_id: &str,
    starting_after: Option<u64>,
) -> Result<reqwest::Response> {
    let mut url = format!(
        "https://api.openai.com/v1/responses/{}?stream=true",
        response_id
    );
    if let Some(seq) = starting_after {
        url.push_str(&format!("&starting_after={}", seq));
    }

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to send poll request to OpenAI")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .context("Failed to read error response")?;
        return Err(anyhow!("OpenAI API error: {} - {}", status, error_text));
    }

    Ok(response)
}

const MAX_STREAM_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;

/// Process request using the responses API with streaming and automatic retry
async fn process_response(
    client: &reqwest::Client,
    api_key: &str,
    system_prompt: &str,
    user_prompt: &str,
    reasoning_effort: &str,
    model: &str,
) -> Result<String> {
    let request_body = serde_json::json!({
        "model": model,
        "instructions": system_prompt,
        "input": [
            {"role": "user", "content": user_prompt}
        ],
        "reasoning": {
            "effort": reasoning_effort
        },
        "stream": true,
        "store": true,
        "background": true,
        "text": {
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "reasoning": {"type": "string"},
                        "substantiveComments": {"type": "boolean"},
                        "summary": {"type": "string"}
                    },
                    "required": ["reasoning", "substantiveComments", "summary"],
                    "additionalProperties": false
                },
                "strict": true,
                "name": "RobocopReview"
            }
        }
    });

    let response = client
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .context("Failed to send request to OpenAI")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .context("Failed to read error response")?;
        return Err(anyhow!("OpenAI API error: {} - {}", status, error_text));
    }

    let mut state = StreamState {
        response_id: None,
        last_sequence: None,
    };

    // Process initial stream
    let mut result = process_sse_stream(response, &mut state).await;

    // Retry loop for interrupted streams
    let mut retries = 0;
    while let StreamResult::Interrupted { error } = result {
        if let Some(ref response_id) = state.response_id {
            if retries >= MAX_STREAM_RETRIES {
                return Err(error.context(format!(
                    "Stream interrupted after {} retries. Response ID: {}",
                    retries, response_id
                )));
            }

            retries += 1;
            eprintln!(
                "Stream interrupted (attempt {}/{}), retrying: {}",
                retries, MAX_STREAM_RETRIES, error
            );

            // Brief delay before retry
            tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;

            // Poll the stored response, resuming from last sequence if available
            match poll_response(client, api_key, response_id, state.last_sequence).await {
                Ok(response) => {
                    result = process_sse_stream(response, &mut state).await;
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "Failed to resume stream. Response ID: {}",
                        response_id
                    )));
                }
            }
        } else {
            // No response ID means we can't retry
            return Err(error.context("Stream interrupted before receiving response ID"));
        }
    }

    match result {
        StreamResult::Completed(text) => Ok(text),
        StreamResult::Failed(error_info) => Err(anyhow!("Response failed: {}", error_info)),
        StreamResult::Interrupted { .. } => unreachable!("handled in retry loop"),
    }
}

/// List available OpenAI models
async fn list_models(client: &reqwest::Client, api_key: &str) -> Result<Vec<ModelInfo>> {
    let response = client
        .get("https://api.openai.com/v1/models")
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to send request to OpenAI")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .context("Failed to read error response")?;
        return Err(anyhow!("OpenAI API error: {} - {}", status, error_text));
    }

    let models_response: ModelsResponse = response
        .json()
        .await
        .context("Failed to parse models response")?;

    Ok(models_response.data)
}

async fn run_review(client: &reqwest::Client, args: ReviewArgs) -> Result<()> {
    // Get git diff (sync operations are fine here)
    let git_data = get_git_diff(&args.default_branch)?;

    // Check if there are any changes
    if git_data.diff.trim().is_empty() {
        println!("No changes detected.");
        return Ok(());
    }
    if git_data.files_changed.is_empty() {
        println!("No changed files detected.");
        return Ok(());
    }

    // Get system prompt
    let system_prompt = get_system_prompt();

    // Build file contents list
    let mut file_contents: Vec<(String, String)> = Vec::new();
    for file in &git_data.files_changed {
        if let Some(content) = read_file(file) {
            file_contents.push((file.clone(), content));
        }
    }

    // Add additional included files (if not already in changed files)
    if !args.include_files.is_empty() {
        for file in &args.include_files {
            if !git_data.files_changed.contains(file) {
                if let Some(content) = read_file(file) {
                    file_contents.push((file.clone(), content));
                }
            }
        }
    }

    // Create user prompt
    let additional_prompt = if args.additional_prompt.is_empty() {
        None
    } else {
        Some(args.additional_prompt.as_str())
    };
    let user_prompt = create_user_prompt(&git_data.diff, &file_contents, additional_prompt);

    // If dry run, just print prompts and exit
    if args.dry_run {
        println!("System prompt:");
        println!("{}", system_prompt);
        println!("\nUser prompt:");
        println!("{}", user_prompt);
        println!("\nModel: {}", args.model);
        return Ok(());
    }

    // Get API key from args or environment (only needed for actual API calls)
    let api_key = args
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("OpenAI API key must be provided via --api-key argument or OPENAI_API_KEY environment variable")?;

    // Process the review
    if args.batch {
        // Use batch API
        let client = OpenAIClient::new(api_key);
        let metadata = ReviewMetadata::from_git_data(&git_data, None);
        let version = robocop_core::get_library_version();

        let batch_id = client
            .process_code_review_batch(
                None, // correlation_id
                &git_data.diff,
                &file_contents,
                &metadata,
                &args.reasoning_effort,
                Some(&version),
                additional_prompt,
                Some(&args.model),
                None, // reconciliation_token - not needed for CLI
                None, // comment_id - not applicable for CLI
                None, // check_run_id - not applicable for CLI
            )
            .await?;

        println!("{}", batch_id);
    } else {
        // Use responses API
        let result = process_response(
            client,
            &api_key,
            &system_prompt,
            &user_prompt,
            &args.reasoning_effort,
            &args.model,
        )
        .await?;

        println!("{}", result.trim());
    }

    Ok(())
}

async fn run_list_models(client: &reqwest::Client, args: ListModelsArgs) -> Result<()> {
    // Get API key from args or environment
    let api_key = args
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("OpenAI API key must be provided via --api-key argument or OPENAI_API_KEY environment variable")?;

    let mut models = list_models(client, &api_key).await?;

    // Sort by id for consistent output
    models.sort_by(|a, b| a.id.cmp(&b.id));

    for model in models {
        println!("{}", model.id);
    }

    Ok(())
}

async fn run_poll(client: &reqwest::Client, args: PollArgs) -> Result<()> {
    // Get API key from args or environment
    let api_key = args
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("OpenAI API key must be provided via --api-key argument or OPENAI_API_KEY environment variable")?;

    // Build URL with stream=true to get SSE events
    let mut url = format!(
        "https://api.openai.com/v1/responses/{}?stream=true",
        args.response_id
    );
    if let Some(seq) = args.sequence {
        url.push_str(&format!("&starting_after={}", seq));
    }

    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to send request to OpenAI")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .context("Failed to read error response")?;
        return Err(anyhow!("OpenAI API error: {} - {}", status, error_text));
    }

    // Stream the response and parse SSE events
    let mut stream = response.bytes_stream();
    let mut sse = SseProcessor::new();
    let mut last_sequence: Option<u64> = None;
    let mut final_result: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read chunk from stream")?;
        sse.push(&chunk);

        // Process all complete events in the buffer
        while let Some(event_result) = sse.next_event() {
            let (event_type, data) = event_result?;

            // Extract and print sequence number with event type
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(seq) = parsed.get("sequence_number").and_then(|s| s.as_u64()) {
                    last_sequence = Some(seq);
                    eprintln!("Sequence: {} ({})", seq, event_type);
                    if args.debug {
                        if let Ok(pretty) = serde_json::to_string_pretty(&parsed) {
                            eprintln!("{}", pretty);
                        }
                    }
                }
            }

            match event_type.as_str() {
                "response.completed" => {
                    final_result = Some(extract_final_text(&data)?);
                }
                "response.failed" => {
                    let error_info = extract_error_info(&data);
                    return Err(anyhow!("Response failed: {}", error_info));
                }
                "error" => {
                    let error_info = extract_error_info(&data);
                    return Err(anyhow!("Stream error: {}", error_info));
                }
                _ => {}
            }
        }
    }

    // Check for incomplete data in buffer
    if sse.has_remaining() {
        let remaining = sse.remaining_as_string();
        eprintln!(
            "Warning: stream ended with unparsed data: {}",
            remaining.chars().take(100).collect::<String>()
        );
    }

    if let Some(seq) = last_sequence {
        eprintln!("Final sequence: {}", seq);
    }

    match final_result {
        Some(text) => {
            println!("{}", text.trim());
            Ok(())
        }
        None => Err(anyhow!(
            "Stream ended without response.completed event. Last sequence: {:?}",
            last_sequence
        )),
    }
}

async fn run_cancel(client: &reqwest::Client, args: CancelArgs) -> Result<()> {
    // Get API key from args or environment
    let api_key = args
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("OpenAI API key must be provided via --api-key argument or OPENAI_API_KEY environment variable")?;

    let url = format!(
        "https://api.openai.com/v1/responses/{}/cancel",
        args.response_id
    );

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .send()
        .await
        .context("Failed to send cancel request to OpenAI")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .context("Failed to read error response")?;
        return Err(anyhow!("OpenAI API error: {} - {}", status, error_text));
    }

    let response_data: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse cancel response")?;

    let status = response_data
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");

    eprintln!("Status: {}", status);

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()
        .context("Failed to create HTTP client")?;

    match cli.command {
        Commands::Review(args) => run_review(&client, args).await,
        Commands::ListModels(args) => run_list_models(&client, args).await,
        Commands::Poll(args) => run_poll(&client, args).await,
        Commands::Cancel(args) => run_cancel(&client, args).await,
    }
}
