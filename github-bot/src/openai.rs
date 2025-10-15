use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};

use crate::recording::{RecordingLogger, RecordingMiddleware, ServiceType, CORRELATION_ID_HEADER};

#[derive(Clone)]
pub struct OpenAIClient {
    client: ClientWithMiddleware,
    api_key: String,
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

#[derive(Debug, Serialize)]
pub struct BatchRequestMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct BatchRequestBody {
    pub model: String,
    pub reasoning_effort: String,
    pub messages: Vec<BatchRequestMessage>,
    pub response_format: ResponseFormat,
}

#[derive(Debug, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub format_type: String,
    pub json_schema: JsonSchema,
}

#[derive(Debug, Serialize)]
pub struct JsonSchema {
    pub schema: Schema,
    pub strict: bool,
    pub name: String,
}

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

#[derive(Debug, Serialize)]
pub struct BatchRequest {
    pub custom_id: String,
    pub method: String,
    pub url: String,
    pub body: BatchRequestBody,
}

#[derive(Debug)]
pub struct ReviewMetadata {
    pub head_hash: String,
    pub merge_base: String,
    pub branch_name: Option<String>,
    pub repo_name: String,
    pub remote_url: Option<String>,
    pub pull_request_url: Option<String>,
}

impl OpenAIClient {
    pub fn new(api_key: String) -> Self {
        Self::new_with_recording(api_key, None)
    }

    pub fn new_with_recording(api_key: String, recording_logger: Option<RecordingLogger>) -> Self {
        let client = create_openai_client(recording_logger);

        Self { client, api_key }
    }

    pub async fn upload_file(
        &self,
        _correlation_id: Option<&str>,
        file_content: &[u8],
        filename: &str,
        purpose: &str,
        expires_after: Option<ExpiresAfter>,
    ) -> Result<FileUploadResponse> {
        let form = reqwest::multipart::Form::new()
            .text("purpose", purpose.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(file_content.to_vec())
                    .file_name(filename.to_string())
                    .mime_str("application/jsonl")?,
            );

        let form = if let Some(expires) = expires_after {
            form.text("expires_after[anchor]", expires.anchor)
                .text("expires_after[seconds]", expires.seconds.to_string())
        } else {
            form
        };

        // Note: Using reqwest directly because reqwest_middleware doesn't support multipart forms
        // This bypasses recording middleware for file uploads, so correlation ID cannot be propagated
        // TODO: Consider implementing a custom solution to record multipart uploads with correlation ID
        let reqwest_client = reqwest::Client::new();
        let response = reqwest_client
            .post("https://api.openai.com/v1/files")
            .header("Authorization", format!("Bearer {}", self.api_key))
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

    fn create_response_format() -> ResponseFormat {
        ResponseFormat {
            format_type: "json_schema".to_string(),
            json_schema: JsonSchema {
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

    fn create_system_prompt() -> String {
        include_str!("../prompt.txt").to_string()
    }

    pub async fn process_code_review_batch(
        &self,
        correlation_id: Option<&str>,
        diff: &str,
        file_contents: &[(String, String)], // (file_path, content) pairs
        metadata: &ReviewMetadata,
        reasoning_effort: &str,
    ) -> Result<String> {
        // Create user prompt following robocop pattern
        let mut user_prompt = format!(
            "Below is a git diff, and then the contents of the altered files after the diff was applied.\n\nDIFF BEGINS:\n{}\nDIFF ENDS\n\nFILE CONTENTS AFTER DIFF APPLIED (omits non-Unicode files and files deleted in the diff):\n\n",
            diff
        );

        for (file_path, content) in file_contents {
            user_prompt.push_str(&format!("\n === {} ===\n\n{}\n\n", file_path, content));
        }

        // Create batch request
        let batch_request = BatchRequest {
            custom_id: "robocop-review-1".to_string(),
            method: "POST".to_string(),
            url: "/v1/chat/completions".to_string(),
            body: BatchRequestBody {
                model: "gpt-5-2025-08-07".to_string(),
                reasoning_effort: reasoning_effort.to_string(),
                messages: vec![
                    BatchRequestMessage {
                        role: "system".to_string(),
                        content: Self::create_system_prompt(),
                    },
                    BatchRequestMessage {
                        role: "user".to_string(),
                        content: user_prompt,
                    },
                ],
                response_format: Self::create_response_format(),
            },
        };

        // Convert to JSONL format (with trailing newline)
        let jsonl_content = format!(
            "{}
",
            serde_json::to_string(&batch_request)?
        );
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
        let version = crate::get_bot_version();
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
        batch_metadata.insert("version".to_string(), version);

        if let Some(url) = &metadata.remote_url {
            batch_metadata.insert("remote_url".to_string(), url.clone());
        }

        if let Some(url) = &metadata.pull_request_url {
            batch_metadata.insert("pull_request_url".to_string(), url.clone());
        }

        // Create batch
        let batch_response = self
            .create_batch(
                correlation_id,
                file_response.id,
                "/v1/chat/completions".to_string(),
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
