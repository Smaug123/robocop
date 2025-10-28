use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::{Client, StatusCode};
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::recording::{RecordingLogger, RecordingMiddleware, ServiceType, CORRELATION_ID_HEADER};

#[derive(Clone)]
pub struct GitHubClient {
    client: ClientWithMiddleware,
    app_id: u64,
    private_key: String,
    token_cache: Arc<RwLock<HashMap<u64, (String, SystemTime)>>>,
}

#[derive(Debug, Serialize)]
pub struct CreateCommentRequest {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub user: CommentUser,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateCommentRequest {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct CommentUser {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct CompareResult {
    pub status: String,
    pub ahead_by: u32,
    pub behind_by: u32,
    pub total_commits: u32,
}

#[derive(Debug, Clone)]
pub struct PullRequestInfo {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
}

#[derive(Debug, Clone)]
pub struct FileContentRequest {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub file_paths: Vec<String>,
    pub sha: String,
}

#[derive(Debug, Clone)]
pub struct FileSizeLimits {
    pub max_file_size: u64,
    pub max_total_size: u64,
}

#[derive(Debug, Serialize)]
struct GitHubAppClaims {
    iss: u64,
    iat: u64,
    exp: u64,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct CompareResponse {
    status: String,
    ahead_by: u32,
    behind_by: u32,
    total_commits: u32,
    files: Vec<FileChange>,
}

#[derive(Debug, Deserialize)]
struct FileChange {
    filename: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct FileContentsResponse {
    content: String,
}

#[derive(Debug, Deserialize)]
struct FileMetadataResponse {
    size: u64,
}

#[derive(Debug, Deserialize)]
struct AppInfoResponse {
    slug: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestResponse {
    pub number: u64,
    pub head: PullRequestRefResponse,
    pub base: PullRequestRefResponse,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestRefResponse {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

impl GitHubClient {
    pub fn new(app_id: u64, private_key: String) -> Self {
        Self::new_with_recording(app_id, private_key, None)
    }

    pub fn new_with_recording(
        app_id: u64,
        private_key: String,
        recording_logger: Option<RecordingLogger>,
    ) -> Self {
        let client = create_github_client(recording_logger);

        Self {
            client,
            app_id,
            private_key,
            token_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn generate_jwt(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("Failed to get current time")?
            .as_secs();

        let claims = GitHubAppClaims {
            iss: self.app_id,
            iat: now - 60,  // Issued 60 seconds ago to account for clock skew
            exp: now + 600, // Expires in 10 minutes
        };

        let header = Header::new(Algorithm::RS256);
        let encoding_key = EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .context("Failed to parse private key")?;

        encode(&header, &claims, &encoding_key).context("Failed to encode JWT")
    }

    async fn get_installation_token(&self, installation_id: u64) -> Result<String> {
        // Check if current token is still valid (with 5 minute buffer)
        {
            let cache = self.token_cache.read().await;
            if let Some((token, expires_at)) = cache.get(&installation_id) {
                if expires_at
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs()
                    > 300
                {
                    return Ok(token.clone());
                }
            }
        }

        let jwt = self.generate_jwt()?;
        let url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            installation_id
        );

        info!("Requesting new installation access token");

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .context("Failed to send installation token request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub App token request failed: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub App token request failed: {} - {}",
                status,
                error_text
            ));
        }

        let token_response: InstallationTokenResponse = response
            .json()
            .await
            .context("Failed to parse installation token response")?;

        // Parse expiration time
        let expires_at = chrono::DateTime::parse_from_rfc3339(&token_response.expires_at)
            .context("Failed to parse token expiration")?
            .with_timezone(&Utc);

        let expires_at_system =
            UNIX_EPOCH + std::time::Duration::from_secs(expires_at.timestamp() as u64);

        // Update the cache with new token
        {
            let mut cache = self.token_cache.write().await;
            cache.insert(
                installation_id,
                (token_response.token.clone(), expires_at_system),
            );
        }

        info!("Successfully obtained installation access token");
        Ok(token_response.token)
    }

    pub async fn post_pr_comment(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        pr_number: u64,
        comment_body: &str,
    ) -> Result<Comment> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}/comments",
            repo_owner, repo_name, pr_number
        );

        info!(
            "Posting comment to PR #{} in {}/{}",
            pr_number, repo_owner, repo_name
        );

        let token = self.get_installation_token(installation_id).await?;
        let request_body = CreateCommentRequest {
            body: comment_body.to_string(),
        };

        let mut request_builder = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json")
            .body(serde_json::to_string(&request_body)?)
            .header("Content-Type", "application/json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send PR comment request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("GitHub API error: {} - {}", status, error_text);
            return Err(anyhow!("GitHub API error: {} - {}", status, error_text));
        }

        let comment: Comment = response
            .json()
            .await
            .context("Failed to parse comment response")?;
        info!("Successfully posted comment with ID: {}", comment.id);

        Ok(comment)
    }

    pub async fn get_diff(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<String> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/compare/{}...{}",
            repo_owner, repo_name, base_sha, head_sha
        );

        info!("Fetching diff from {}...{}", base_sha, head_sha);

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3.diff");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send diff request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error fetching diff: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error fetching diff: {} - {}",
                status,
                error_text
            ));
        }

        let diff = response
            .text()
            .await
            .context("Failed to read diff response body")?;
        info!("Successfully fetched diff ({} bytes)", diff.len());

        Ok(diff)
    }

    pub async fn get_changed_files_from_diff(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<String>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/compare/{}...{}",
            repo_owner, repo_name, base_sha, head_sha
        );

        info!("Fetching changed files from {}...{}", base_sha, head_sha);

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send compare request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error fetching compare: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error fetching compare: {} - {}",
                status,
                error_text
            ));
        }

        let compare_response: CompareResponse = response
            .json()
            .await
            .context("Failed to parse compare response")?;
        let mut changed_files = Vec::new();

        for file in compare_response.files {
            if file.status != "removed" {
                changed_files.push(file.filename);
            }
        }

        info!("Found {} changed files", changed_files.len());
        Ok(changed_files)
    }

    async fn get_file_size(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        file_path: &str,
        sha: &str,
    ) -> Result<u64> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
            repo_owner, repo_name, file_path, sha
        );

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send file metadata request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            return Err(anyhow!(
                "GitHub API error fetching file metadata: {} - {}",
                status,
                error_text
            ));
        }

        let file_response: FileMetadataResponse = response
            .json()
            .await
            .context("Failed to parse file metadata response")?;
        Ok(file_response.size)
    }

    pub async fn get_multiple_file_contents_with_limits(
        &self,
        correlation_id: Option<&str>,
        request: &FileContentRequest,
        limits: &FileSizeLimits,
    ) -> Result<(Vec<(String, String)>, Vec<String>)> {
        let mut results = Vec::new();
        let mut skipped_files = Vec::new();
        let mut total_downloaded = 0u64;

        for file_path in &request.file_paths {
            // Get file size first
            let file_size = match self
                .get_file_size(
                    correlation_id,
                    request.installation_id,
                    &request.repo_owner,
                    &request.repo_name,
                    file_path,
                    &request.sha,
                )
                .await
            {
                Ok(size) => size,
                Err(e) => {
                    warn!("Failed to get size for {}: {}", file_path, e);
                    skipped_files.push(format!("{} (size check failed)", file_path));
                    continue;
                }
            };

            // Skip if file is too large
            if file_size > limits.max_file_size {
                warn!("Skipping large file {}: {} bytes", file_path, file_size);
                skipped_files.push(format!("{} ({} bytes)", file_path, file_size));
                continue;
            }

            // Skip if would exceed total limit
            if total_downloaded + file_size > limits.max_total_size {
                warn!(
                    "Stopping downloads at {}MB limit (would exceed with {})",
                    limits.max_total_size / 1_000_000,
                    file_path
                );
                skipped_files.push(format!("{} (would exceed total limit)", file_path));
                break;
            }

            // Download content
            match self
                .get_file_contents(
                    correlation_id,
                    request.installation_id,
                    &request.repo_owner,
                    &request.repo_name,
                    file_path,
                    &request.sha,
                )
                .await
            {
                Ok(content) => {
                    total_downloaded += content.len() as u64;
                    results.push((file_path.clone(), content));
                }
                Err(e) => {
                    warn!("Failed to fetch content for {}: {}", file_path, e);
                    skipped_files.push(format!("{} (download failed)", file_path));
                }
            }
        }

        info!(
            "Downloaded {}/{} files ({} bytes), skipped {}",
            results.len(),
            request.file_paths.len(),
            total_downloaded,
            skipped_files.len()
        );
        Ok((results, skipped_files))
    }

    pub async fn get_multiple_file_contents(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        file_paths: &[String],
        sha: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut results = Vec::new();

        for file_path in file_paths {
            match self
                .get_file_contents(
                    correlation_id,
                    installation_id,
                    repo_owner,
                    repo_name,
                    file_path,
                    sha,
                )
                .await
            {
                Ok(content) => {
                    results.push((file_path.clone(), content));
                }
                Err(e) => {
                    // Log the error but continue with other files
                    // This handles cases where files are binary, deleted, or inaccessible
                    warn!("Failed to fetch content for {}: {}", file_path, e);
                }
            }
        }

        info!(
            "Successfully collected content for {}/{} files",
            results.len(),
            file_paths.len()
        );
        Ok(results)
    }

    pub async fn get_file_contents(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        file_path: &str,
        sha: &str,
    ) -> Result<String> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
            repo_owner, repo_name, file_path, sha
        );

        info!("Fetching file contents: {} at {}", file_path, sha);

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send file contents request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error fetching file: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error fetching file: {} - {}",
                status,
                error_text
            ));
        }

        let file_response: FileContentsResponse = response
            .json()
            .await
            .context("Failed to parse file contents response")?;

        let decoded = general_purpose::STANDARD
            .decode(file_response.content.replace('\n', ""))
            .context("Failed to decode base64 file content")?;
        let content_str = String::from_utf8(decoded).context("File content is not valid UTF-8")?;
        info!(
            "Successfully fetched file contents ({} bytes)",
            content_str.len()
        );
        Ok(content_str)
    }

    pub async fn get_pr_comments(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        pr_number: u64,
    ) -> Result<Vec<Comment>> {
        let mut all_comments = Vec::new();
        let mut page = 1;
        let per_page = 100; // Max per page for better efficiency

        info!(
            "Fetching comments for PR #{} in {}/{}",
            pr_number, repo_owner, repo_name
        );

        loop {
            let url = format!(
                "https://api.github.com/repos/{}/{}/issues/{}/comments?page={}&per_page={}",
                repo_owner, repo_name, pr_number, page, per_page
            );

            // Fetch fresh token for each page to handle expiry
            let token = self.get_installation_token(installation_id).await?;

            let mut request_builder = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Accept", "application/vnd.github.v3+json");

            if let Some(cid) = correlation_id {
                request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
            }

            let response = request_builder
                .send()
                .await
                .context("Failed to send PR comments request")?;

            if !response.status().is_success() {
                let status = response.status();
                let error_text = response.text().await.unwrap_or_default();

                // If we get a 401, it might be due to token expiry - try once more with fresh token
                if status == StatusCode::UNAUTHORIZED {
                    warn!("Got 401 error on page {}, retrying with fresh token", page);

                    // Force refresh the token by clearing cached one
                    {
                        let mut cache = self.token_cache.write().await;
                        cache.remove(&installation_id);
                    }

                    let fresh_token = self.get_installation_token(installation_id).await?;

                    let mut retry_request_builder = self
                        .client
                        .get(&url)
                        .header("Authorization", format!("Bearer {}", fresh_token))
                        .header("Accept", "application/vnd.github.v3+json");

                    if let Some(cid) = correlation_id {
                        retry_request_builder =
                            retry_request_builder.header(CORRELATION_ID_HEADER, cid);
                    }

                    let retry_response = retry_request_builder
                        .send()
                        .await
                        .context("Failed to send retry PR comments request")?;

                    if !retry_response.status().is_success() {
                        let retry_status = retry_response.status();
                        let retry_error_text = retry_response.text().await.unwrap_or_default();
                        error!(
                            "GitHub API error fetching comments after retry: {} - {}",
                            retry_status, retry_error_text
                        );
                        return Err(anyhow!(
                            "GitHub API error fetching comments after retry: {} - {}",
                            retry_status,
                            retry_error_text
                        ));
                    }

                    let comments: Vec<Comment> = retry_response
                        .json()
                        .await
                        .context("Failed to parse retry comments response")?;
                    let comments_count = comments.len();
                    all_comments.extend(comments);

                    // If we got fewer comments than per_page, we've reached the last page
                    if comments_count < per_page {
                        break;
                    }
                } else {
                    error!(
                        "GitHub API error fetching comments: {} - {}",
                        status, error_text
                    );
                    return Err(anyhow!(
                        "GitHub API error fetching comments: {} - {}",
                        status,
                        error_text
                    ));
                }
            } else {
                let comments: Vec<Comment> = response
                    .json()
                    .await
                    .context("Failed to parse comments response")?;
                let comments_count = comments.len();
                all_comments.extend(comments);

                // If we got fewer comments than per_page, we've reached the last page
                if comments_count < per_page {
                    break;
                }
            }

            page += 1;
        }

        info!(
            "Found {} total comments on PR #{}",
            all_comments.len(),
            pr_number
        );
        Ok(all_comments)
    }

    pub async fn update_comment(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        comment_id: u64,
        comment_body: &str,
    ) -> Result<Comment> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/comments/{}",
            repo_owner, repo_name, comment_id
        );

        info!(
            "Updating comment {} in {}/{}",
            comment_id, repo_owner, repo_name
        );

        let token = self.get_installation_token(installation_id).await?;
        let request_body = UpdateCommentRequest {
            body: comment_body.to_string(),
        };

        let mut request_builder = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json")
            .body(serde_json::to_string(&request_body)?)
            .header("Content-Type", "application/json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send update comment request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error updating comment: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error updating comment: {} - {}",
                status,
                error_text
            ));
        }

        let comment: Comment = response
            .json()
            .await
            .context("Failed to parse updated comment response")?;
        info!("Successfully updated comment with ID: {}", comment.id);

        Ok(comment)
    }

    async fn get_bot_user(&self) -> Result<String> {
        let url = "https://api.github.com/app";

        let jwt = self.generate_jwt()?;
        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .context("Failed to send app info request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error fetching app info: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error fetching app info: {} - {}",
                status,
                error_text
            ));
        }

        let app_info: AppInfoResponse = response
            .json()
            .await
            .context("Failed to parse app info response")?;
        Ok(format!("{}[bot]", app_info.slug))
    }

    pub async fn manage_robocop_comment(
        &self,
        correlation_id: Option<&str>,
        pr_info: &PullRequestInfo,
        content: &str,
        version: &str,
    ) -> Result<u64> {
        let magic_string = format!("<!-- robocop({}) -->", version);
        let full_comment = format!("{}\n\n{}", magic_string, content);

        // Get bot username
        let bot_username = self.get_bot_user().await?;

        // Get all comments on the PR
        let comments = self
            .get_pr_comments(
                correlation_id,
                pr_info.installation_id,
                &pr_info.repo_owner,
                &pr_info.repo_name,
                pr_info.pr_number,
            )
            .await?;

        // Look for existing robocop comment from this bot
        for comment in comments {
            if comment.user.login == bot_username && comment.body.starts_with("<!-- robocop(") {
                info!("Found existing robocop comment {}, updating...", comment.id);
                self.update_comment(
                    correlation_id,
                    pr_info.installation_id,
                    &pr_info.repo_owner,
                    &pr_info.repo_name,
                    comment.id,
                    &full_comment,
                )
                .await?;
                return Ok(comment.id);
            }
        }

        // No existing comment found, create new one
        info!("No existing robocop comment found, creating new one...");
        let new_comment = self
            .post_pr_comment(
                correlation_id,
                pr_info.installation_id,
                &pr_info.repo_owner,
                &pr_info.repo_name,
                pr_info.pr_number,
                &full_comment,
            )
            .await?;
        Ok(new_comment.id)
    }

    pub async fn get_pull_request(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        pr_number: u64,
    ) -> Result<PullRequestResponse> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls/{}",
            repo_owner, repo_name, pr_number
        );

        info!(
            "Fetching PR #{} from {}/{}",
            pr_number, repo_owner, repo_name
        );

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send get pull request request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!("GitHub API error fetching PR: {} - {}", status, error_text);
            return Err(anyhow!(
                "GitHub API error fetching PR: {} - {}",
                status,
                error_text
            ));
        }

        let pr_response: PullRequestResponse = response
            .json()
            .await
            .context("Failed to parse pull request response")?;

        info!(
            "Successfully fetched PR #{} (head: {}, base: {})",
            pr_response.number, pr_response.head.sha, pr_response.base.sha
        );

        Ok(pr_response)
    }

    pub async fn compare_commits(
        &self,
        correlation_id: Option<&str>,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<CompareResult> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/compare/{}...{}",
            repo_owner, repo_name, base_sha, head_sha
        );

        info!(
            "Comparing commits {} to {} in {}/{}",
            base_sha, head_sha, repo_owner, repo_name
        );

        let token = self.get_installation_token(installation_id).await?;
        let mut request_builder = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(cid) = correlation_id {
            request_builder = request_builder.header(CORRELATION_ID_HEADER, cid);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to send compare commits request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .context("Failed to read error response body")?;
            error!(
                "GitHub API error comparing commits: {} - {}",
                status, error_text
            );
            return Err(anyhow!(
                "GitHub API error comparing commits: {} - {}",
                status,
                error_text
            ));
        }

        let compare_response: CompareResponse = response
            .json()
            .await
            .context("Failed to parse compare commits response")?;

        let result = CompareResult {
            status: compare_response.status,
            ahead_by: compare_response.ahead_by,
            behind_by: compare_response.behind_by,
            total_commits: compare_response.total_commits,
        };

        info!(
            "Compare result: {} (ahead: {}, behind: {}, total: {})",
            result.status, result.ahead_by, result.behind_by, result.total_commits
        );

        Ok(result)
    }
}

/// Check if a PR description or comment contains the disable-reviews marker
///
/// This performs a case-insensitive search for the @smaug123-robocop disable-reviews
/// pattern anywhere in the text (not just at line start, since PR descriptions
/// are not constrained like comments).
pub fn contains_disable_reviews_marker(text: &str) -> bool {
    for line in text.lines() {
        if line
            .trim()
            .to_lowercase()
            .contains("@smaug123-robocop disable-reviews")
        {
            return true;
        }
    }
    false
}

pub fn create_github_client(recording_logger: Option<RecordingLogger>) -> ClientWithMiddleware {
    use reqwest_middleware::ClientBuilder;

    let client = Client::builder()
        .user_agent("Smaug123-robocop/0.1.0")
        .build()
        .expect("Failed to create HTTP client");

    let mut builder = ClientBuilder::new(client);

    if let Some(logger) = recording_logger {
        let recording_middleware = RecordingMiddleware::new(logger, ServiceType::GitHub);
        builder = builder.with(recording_middleware);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_disable_reviews_marker_present() {
        let text = "This is a PR\n\n@smaug123-robocop disable-reviews\n\nPlease don't review";
        assert!(contains_disable_reviews_marker(text));
    }

    #[test]
    fn test_contains_disable_reviews_marker_case_insensitive() {
        let text = "@SMAUG123-ROBOCOP DISABLE-REVIEWS";
        assert!(contains_disable_reviews_marker(text));

        let text2 = "@Smaug123-Robocop Disable-Reviews";
        assert!(contains_disable_reviews_marker(text2));
    }

    #[test]
    fn test_contains_disable_reviews_marker_absent() {
        let text = "Just a regular PR description";
        assert!(!contains_disable_reviews_marker(text));

        let text2 = "Mentions @smaug123-robocop but not the disable command";
        assert!(!contains_disable_reviews_marker(text2));
    }

    #[test]
    fn test_contains_disable_reviews_marker_with_whitespace() {
        let text = "  @smaug123-robocop disable-reviews  ";
        assert!(contains_disable_reviews_marker(text));
    }
}
