use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::review::{create_user_prompt, get_system_prompt, ReviewMetadata};

/// Synchronous OpenAI client for code review batch processing
#[derive(Clone)]
pub struct OpenAIClient {
    client: reqwest::blocking::Client,
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

impl OpenAIClient {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Smaug123-robocop/0.1.0")
            .build()
            .expect("Failed to create HTTP client");

        Self { client, api_key }
    }

    pub fn upload_file(
        &self,
        file_content: &[u8],
        filename: &str,
        purpose: &str,
        expires_after: Option<ExpiresAfter>,
    ) -> Result<FileUploadResponse> {
        let mut form = reqwest::blocking::multipart::Form::new()
            .text("purpose", purpose.to_string())
            .part(
                "file",
                reqwest::blocking::multipart::Part::bytes(file_content.to_vec())
                    .file_name(filename.to_string())
                    .mime_str("application/jsonl")?,
            );

        if let Some(expires) = expires_after {
            form = form
                .text("expires_after[anchor]", expires.anchor)
                .text("expires_after[seconds]", expires.seconds.to_string());
        }

        let response = self
            .client
            .post("https://api.openai.com/v1/files")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .context("Failed to upload file to OpenAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "OpenAI Files API error: {} - {}",
                status,
                error_text
            ));
        }

        let upload_response: FileUploadResponse = response
            .json()
            .context("Failed to parse file upload response")?;

        Ok(upload_response)
    }

    pub fn create_batch(
        &self,
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

        let response = self
            .client
            .post("https://api.openai.com/v1/batches")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&request_body)?)
            .send()
            .context("Failed to create batch request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "OpenAI Batches API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .context("Failed to parse batch create response")?;

        Ok(batch_response)
    }

    pub fn get_batch(&self, batch_id: &str) -> Result<BatchResponse> {
        let url = format!("https://api.openai.com/v1/batches/{}", batch_id);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .send()
            .context("Failed to get batch status")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "OpenAI Get Batch API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .context("Failed to parse batch status response")?;
        Ok(batch_response)
    }

    pub fn cancel_batch(&self, batch_id: &str) -> Result<BatchResponse> {
        let url = format!("https://api.openai.com/v1/batches/{}/cancel", batch_id);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .send()
            .context("Failed to cancel batch")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "OpenAI Cancel Batch API error: {} - {}",
                status,
                error_text
            ));
        }

        let batch_response: BatchResponse = response
            .json()
            .context("Failed to parse batch cancel response")?;

        Ok(batch_response)
    }

    pub fn download_batch_output(&self, file_id: &str) -> Result<String> {
        let url = format!("https://api.openai.com/v1/files/{}/content", file_id);

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .context("Failed to download batch output")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "OpenAI Download File API error: {} - {}",
                status,
                error_text
            ));
        }

        let content = response
            .text()
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

    /// Process a code review batch request
    /// Returns the batch ID
    pub fn process_code_review_batch(
        &self,
        diff: &str,
        file_contents: &[(String, String)], // (file_path, content) pairs
        metadata: &ReviewMetadata,
        reasoning_effort: &str,
        version: Option<&str>,
        additional_prompt: Option<&str>,
    ) -> Result<String> {
        // Create user prompt
        let user_prompt = create_user_prompt(diff, file_contents, additional_prompt);

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
                        content: get_system_prompt(),
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
        let jsonl_content = format!("{}\n", serde_json::to_string(&batch_request)?);
        let jsonl_bytes = jsonl_content.as_bytes();

        // Upload batch file
        let file_response = self.upload_file(
            jsonl_bytes,
            "batch_request.jsonl",
            "batch",
            None, // No expiration for batch files
        )?;

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

        if let Some(ver) = version {
            batch_metadata.insert("version".to_string(), ver.to_string());
        }

        if let Some(url) = &metadata.remote_url {
            batch_metadata.insert("remote_url".to_string(), url.clone());
        }

        if let Some(url) = &metadata.pull_request_url {
            batch_metadata.insert("pull_request_url".to_string(), url.clone());
        }

        // Create batch
        let batch_response = self.create_batch(
            file_response.id,
            "/v1/chat/completions".to_string(),
            "24h".to_string(),
            Some(batch_metadata),
            None, // No output expiration
        )?;

        Ok(batch_response.id)
    }
}
