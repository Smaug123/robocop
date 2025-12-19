use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};

use crate::recording::{
    Direction, EventType, RecordedEvent, RecordingLogger, RecordingMiddleware, ServiceType,
    CORRELATION_ID_HEADER,
};
use crate::review::{create_user_prompt, get_system_prompt, ReviewMetadata};

/// Async OpenAI client for code review batch processing
#[derive(Clone)]
pub struct OpenAIClient {
    client: ClientWithMiddleware,
    api_key: String,
    recording_logger: Option<RecordingLogger>,
}

#[derive(Debug, Deserialize)]
pub struct FileUploadResponse {
    pub id: String,
    pub object: String,
    pub bytes: u64,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub filename: String,
    pub purpose: String,
}

#[derive(Debug, Serialize)]
pub struct ExpiresAfter {
    pub anchor: String,
    pub seconds: u32,
}

#[derive(Debug, Serialize)]
pub struct BatchCreateRequest {
    pub input_file_id: String,
    pub endpoint: String,
    pub completion_window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_expires_after: Option<ExpiresAfter>,
}

#[derive(Debug, Deserialize)]
pub struct RequestCounts {
    pub total: u32,
    pub completed: u32,
    pub failed: u32,
}

#[derive(Debug, Deserialize)]
pub struct BatchResponse {
    pub id: String,
    pub object: String,
    pub endpoint: String,
    pub errors: Option<serde_json::Value>,
    pub input_file_id: String,
    pub completion_window: String,
    pub status: String,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
    pub created_at: u64,
    pub in_progress_at: Option<u64>,
    pub expires_at: Option<u64>,
    pub finalizing_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub failed_at: Option<u64>,
    pub expired_at: Option<u64>,
    pub cancelling_at: Option<u64>,
    pub cancelled_at: Option<u64>,
    pub request_counts: RequestCounts,
    pub metadata: Option<HashMap<String, String>>,
}

/// Input message for responses API.
/// See https://platform.openai.com/docs/guides/text?api-mode=responses
#[derive(Debug, Serialize)]
pub struct ResponsesInputMessage {
    pub role: String,
    pub content: String,
}

/// Reasoning configuration for responses API
#[derive(Debug, Serialize)]
pub struct ReasoningConfig {
    pub effort: String,
}

/// Text format configuration for responses API
#[derive(Debug, Serialize)]
pub struct TextFormat {
    pub format: TextFormatType,
}

/// Text format type for JSON schema
#[derive(Debug, Serialize)]
pub struct TextFormatType {
    #[serde(rename = "type")]
    pub format_type: String,
    pub schema: Schema,
    pub name: String,
    pub strict: bool,
}

/// Schema definition for structured output
#[derive(Debug, Serialize)]
pub struct Schema {
    #[serde(rename = "type")]
    pub schema_type: String,
    pub properties: SchemaProperties,
    pub required: Vec<String>,
    #[serde(rename = "additionalProperties")]
    pub additional_properties: bool,
}

#[derive(Debug, Serialize)]
pub struct SchemaProperties {
    pub reasoning: SchemaProperty,
    #[serde(rename = "substantiveComments")]
    pub substantive_comments: SchemaProperty,
    pub summary: SchemaProperty,
}

#[derive(Debug, Serialize)]
pub struct SchemaProperty {
    #[serde(rename = "type")]
    pub property_type: String,
}

/// Request body for the responses API
#[derive(Debug, Serialize)]
pub struct BatchRequestBody {
    pub model: String,
    pub instructions: String,
    pub input: Vec<ResponsesInputMessage>,
    pub reasoning: ReasoningConfig,
    pub text: TextFormat,
}

#[derive(Debug, Serialize)]
pub struct BatchRequest {
    pub custom_id: String,
    pub method: String,
    pub url: String,
    pub body: BatchRequestBody,
}

impl OpenAIClient {
    /// Create a new OpenAI client without recording
    pub fn new(api_key: String) -> Self {
        Self::new_with_recording(api_key, None)
    }

    /// Create a new OpenAI client with optional recording support
    pub fn new_with_recording(api_key: String, recording_logger: Option<RecordingLogger>) -> Self {
        let client = create_openai_client(recording_logger.clone());
        Self {
            client,
            api_key,
            recording_logger,
        }
    }

    /// Helper to get or generate a correlation ID
    fn get_or_generate_correlation_id(correlation_id: Option<&str>) -> String {
        correlation_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    }

    /// Record a file upload request (manual recording since middleware doesn't support multipart)
    fn record_upload_request(
        &self,
        correlation_id: &str,
        filename: &str,
        file_size: usize,
        purpose: &str,
    ) {
        if let Some(logger) = &self.recording_logger {
            let event = RecordedEvent {
                timestamp: chrono::Utc::now().to_rfc3339(),
                correlation_id: correlation_id.to_string(),
                event_type: EventType::OpenAiApiCall,
                direction: Direction::Request,
                operation: "POST /v1/files".to_string(),
                data: serde_json::json!({
                    "method": "POST",
                    "url": "https://api.openai.com/v1/files",
                    "filename": filename,
                    "file_size_bytes": file_size,
                    "purpose": purpose,
                    "note": "multipart/form-data request body not captured"
                }),
                metadata: HashMap::new(),
            };
            logger.record(event);
        }
    }

    /// Record a file upload response
    fn record_upload_response(&self, correlation_id: &str, response: &FileUploadResponse) {
        if let Some(logger) = &self.recording_logger {
            let event = RecordedEvent {
                timestamp: chrono::Utc::now().to_rfc3339(),
                correlation_id: correlation_id.to_string(),
                event_type: EventType::OpenAiApiCall,
                direction: Direction::Response,
                operation: "response_200".to_string(),
                data: serde_json::json!({
                    "status_code": 200,
                    "file_id": response.id,
                    "filename": response.filename,
                    "bytes": response.bytes,
                    "purpose": response.purpose
                }),
                metadata: HashMap::new(),
            };
            logger.record(event);
        }
    }

    /// Record a file upload error
    fn record_upload_error(&self, correlation_id: &str, status: u16, error_text: &str) {
        if let Some(logger) = &self.recording_logger {
            let event = RecordedEvent {
                timestamp: chrono::Utc::now().to_rfc3339(),
                correlation_id: correlation_id.to_string(),
                event_type: EventType::OpenAiApiCall,
                direction: Direction::Response,
                operation: format!("response_{}", status),
                data: serde_json::json!({
                    "status_code": status,
                    "error": error_text
                }),
                metadata: HashMap::new(),
            };
            logger.record(event);
        }
    }

    pub async fn upload_file(
        &self,
        correlation_id: Option<&str>,
        file_content: &[u8],
        filename: &str,
        purpose: &str,
        expires_after: Option<ExpiresAfter>,
    ) -> Result<FileUploadResponse> {
        let correlation_id = Self::get_or_generate_correlation_id(correlation_id);

        // Record request (manual recording since middleware doesn't support multipart)
        self.record_upload_request(&correlation_id, filename, file_content.len(), purpose);

        let mut form = reqwest::multipart::Form::new()
            .text("purpose", purpose.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(file_content.to_vec())
                    .file_name(filename.to_string())
                    .mime_str("application/jsonl")?,
            );

        if let Some(expires) = expires_after {
            form = form
                .text("expires_after[anchor]", expires.anchor)
                .text("expires_after[seconds]", expires.seconds.to_string());
        }

        // Note: Using reqwest directly for multipart because reqwest_middleware doesn't support it well
        let reqwest_client = reqwest::Client::new();
        let response = reqwest_client
            .post("https://api.openai.com/v1/files")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header(CORRELATION_ID_HEADER, &correlation_id)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload file to OpenAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("OpenAI Files API error: {} - {}", status, error_text);

            // Record error response
            self.record_upload_error(&correlation_id, status.as_u16(), &error_text);

            return Err(anyhow!(
                "OpenAI Files API error: {} - {}",
                status,
                error_text
            ));
        }

        let upload_response: FileUploadResponse = response
            .json()
            .await
            .context("Failed to parse file upload response")?;
        info!(
            "Successfully uploaded file: {} (id: {}, {} bytes)",
            upload_response.filename, upload_response.id, upload_response.bytes
        );

        // Record successful response
        self.record_upload_response(&correlation_id, &upload_response);

        Ok(upload_response)
    }

    pub async fn create_batch(
        &self,
        correlation_id: Option<&str>,
        input_file_id: String,
        endpoint: String,
        completion_window: String,
        metadata: Option<HashMap<String, String>>,
        output_expires_after: Option<ExpiresAfter>,
    ) -> Result<BatchResponse> {
        let request_body = BatchCreateRequest {
            input_file_id,
            endpoint,
            completion_window,
            metadata,
            output_expires_after,
        };

        let mut request_builder = self
            .client
            .post("https://api.openai.com/v1/batches")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&request_body)?);

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to create batch request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("OpenAI Batches API error: {} - {}", status, error_text);
            return Err(anyhow!(
                "OpenAI Batches API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .await
            .context("Failed to parse batch create response")?;
        info!(
            "Successfully created batch: {} (status: {})",
            batch_response.id, batch_response.status
        );

        Ok(batch_response)
    }

    pub async fn get_batch(
        &self,
        correlation_id: Option<&str>,
        batch_id: &str,
    ) -> Result<BatchResponse> {
        let url = format!("https://api.openai.com/v1/batches/{}", batch_id);

        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to get batch status")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("OpenAI Get Batch API error: {} - {}", status, error_text);
            return Err(anyhow!(
                "OpenAI Get Batch API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .await
            .context("Failed to parse batch status response")?;
        Ok(batch_response)
    }

    pub async fn cancel_batch(
        &self,
        correlation_id: Option<&str>,
        batch_id: &str,
    ) -> Result<BatchResponse> {
        let url = format!("https://api.openai.com/v1/batches/{}/cancel", batch_id);

        info!("Cancelling batch: {}", batch_id);

        let mut request_builder = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to cancel batch")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("OpenAI Cancel Batch API error: {} - {}", status, error_text);
            return Err(anyhow!(
                "OpenAI Cancel Batch API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .await
            .context("Failed to parse batch cancel response")?;
        info!(
            "Successfully cancelled batch: {} (status: {})",
            batch_response.id, batch_response.status
        );

        Ok(batch_response)
    }

    pub async fn download_batch_output(
        &self,
        correlation_id: Option<&str>,
        file_id: &str,
    ) -> Result<String> {
        let url = format!("https://api.openai.com/v1/files/{}/content", file_id);

        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key));

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to download batch output")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "OpenAI Download File API error: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "OpenAI Download File API error: {} - {}",
                status,
                error_text
            ));
        }

        let content = response
            .text()
            .await
            .context("Failed to read batch output content")?;
        Ok(content)
    }

    pub fn create_text_format() -> TextFormat {
        TextFormat {
            format: TextFormatType {
                format_type: "json_schema".to_string(),
                schema: Schema {
                    schema_type: "object".to_string(),
                    properties: SchemaProperties {
                        reasoning: SchemaProperty {
                            property_type: "string".to_string(),
                        },
                        substantive_comments: SchemaProperty {
                            property_type: "boolean".to_string(),
                        },
                        summary: SchemaProperty {
                            property_type: "string".to_string(),
                        },
                    },
                    required: vec![
                        "reasoning".to_string(),
                        "substantiveComments".to_string(),
                        "summary".to_string(),
                    ],
                    additional_properties: false,
                },
                strict: true,
                name: "RobocopReview".to_string(),
            },
        }
    }

    /// Process a code review batch request
    /// Returns the batch ID
    #[allow(clippy::too_many_arguments)]
    pub async fn process_code_review_batch(
        &self,
        correlation_id: Option<&str>,
        diff: &str,
        file_contents: &[(String, String)], // (file_path, content) pairs
        metadata: &ReviewMetadata,
        reasoning_effort: &str,
        version: Option<&str>,
        additional_prompt: Option<&str>,
        model: Option<&str>,
    ) -> Result<String> {
        // Create user prompt
        let user_prompt = create_user_prompt(diff, file_contents, additional_prompt);

        // Create batch request using the responses API format
        let batch_request = BatchRequest {
            custom_id: "robocop-review-1".to_string(),
            method: "POST".to_string(),
            url: "/v1/responses".to_string(),
            body: BatchRequestBody {
                model: model.unwrap_or("gpt-5.2-2025-12-11").to_string(),
                instructions: get_system_prompt(),
                input: vec![ResponsesInputMessage {
                    role: "user".to_string(),
                    content: user_prompt,
                }],
                reasoning: ReasoningConfig {
                    effort: reasoning_effort.to_string(),
                },
                text: Self::create_text_format(),
            },
        };

        // Convert to JSONL format (with trailing newline)
        let jsonl_content = format!("{}\n", serde_json::to_string(&batch_request)?);
        let jsonl_bytes = jsonl_content.as_bytes();

        info!("Created batch request ({} bytes JSONL)", jsonl_bytes.len());

        // Upload batch file
        let file_response = self
            .upload_file(
                correlation_id,
                jsonl_bytes,
                "batch_request.jsonl",
                "batch",
                None, // No expiration for batch files
            )
            .await?;

        info!("Uploaded batch file: {}", file_response.id);

        // Create batch metadata
        let mut batch_metadata = HashMap::new();
        batch_metadata.insert(
            "description".to_string(),
            "robocop code review tool".to_string(),
        );
        batch_metadata.insert("source_commit".to_string(), metadata.head_hash.clone());
        batch_metadata.insert("target_commit".to_string(), metadata.merge_base.clone());
        batch_metadata.insert(
            "branch".to_string(),
            metadata
                .branch_name
                .clone()
                .unwrap_or("<no branch>".to_string()),
        );
        batch_metadata.insert("metadata_schema".to_string(), "1".to_string());
        batch_metadata.insert("repo_name".to_string(), metadata.repo_name.clone());
        batch_metadata.insert(
            "model".to_string(),
            model.unwrap_or("gpt-5-2025-08-07").to_string(),
        );
        batch_metadata.insert("reasoning_effort".to_string(), reasoning_effort.to_string());

        if let Some(ver) = version {
            batch_metadata.insert("version".to_string(), ver.to_string());
        }

        if let Some(url) = &metadata.remote_url {
            batch_metadata.insert("remote_url".to_string(), url.clone());
        }

        if let Some(url) = &metadata.pull_request_url {
            batch_metadata.insert("pull_request_url".to_string(), url.clone());
        }

        // Create batch using responses API endpoint
        let batch_response = self
            .create_batch(
                correlation_id,
                file_response.id,
                "/v1/responses".to_string(),
                "24h".to_string(),
                Some(batch_metadata),
                None, // No output expiration
            )
            .await?;

        info!(
            "Created batch: {} (status: {})",
            batch_response.id, batch_response.status
        );

        Ok(batch_response.id)
    }
}

/// Create an OpenAI HTTP client with optional recording middleware
pub fn create_openai_client(recording_logger: Option<RecordingLogger>) -> ClientWithMiddleware {
    use reqwest_middleware::ClientBuilder;

    let client = Client::builder()
        .user_agent("Smaug123-robocop/0.1.0")
        .build()
        .expect("Failed to create HTTP client");

    let mut builder = ClientBuilder::new(client);

    if let Some(logger) = recording_logger {
        let recording_middleware = RecordingMiddleware::new(logger, ServiceType::OpenAi);
        builder = builder.with(recording_middleware);
    }

    builder.build()
}
