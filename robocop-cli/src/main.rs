use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use robocop_core::{create_user_prompt, get_system_prompt, GitData, OpenAIClient, ReviewMetadata};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const DEFAULT_MODEL: &str = "gpt-5.2-2025-12-11 ";

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

/// Response from OpenAI responses API
#[derive(serde::Deserialize, Debug)]
struct ResponsesApiResponse {
    status: String,
    output: Vec<ResponseOutput>,
}

#[derive(serde::Deserialize, Debug)]
struct ResponseOutput {
    #[serde(rename = "type")]
    output_type: String,
    /// Content is optional because some output types (like reasoning) don't have it
    #[serde(default)]
    content: Vec<ResponseContent>,
}

#[derive(serde::Deserialize, Debug)]
struct ResponseContent {
    #[serde(rename = "type")]
    content_type: String,
    /// Text is optional because some content types (e.g., refusal) may not have it
    text: Option<String>,
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

/// Process request using the responses API
async fn process_response(
    api_key: &str,
    system_prompt: &str,
    user_prompt: &str,
    reasoning_effort: &str,
    model: &str,
) -> Result<String> {
    let client = reqwest::Client::new();

    let request_body = serde_json::json!({
        "model": model,
        "instructions": system_prompt,
        "input": [
            {"role": "user", "content": user_prompt}
        ],
        "reasoning": {
            "effort": reasoning_effort
        },
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

    let api_response: ResponsesApiResponse = response
        .json()
        .await
        .context("Failed to parse responses API response")?;

    if api_response.status != "completed" {
        return Err(anyhow!(
            "Unexpected response status: {}",
            api_response.status
        ));
    }

    // Find the message output (there may be other outputs like reasoning)
    let message_output = api_response
        .output
        .iter()
        .find(|o| o.output_type == "message")
        .context("No message output found in response")?;

    // Find the output_text content with text
    let text_content = message_output
        .content
        .iter()
        .find(|c| c.content_type == "output_text" && c.text.is_some())
        .context("No output_text content found in message")?;

    Ok(text_content.text.clone().unwrap())
}

/// List available OpenAI models
async fn list_models(api_key: &str) -> Result<Vec<ModelInfo>> {
    let client = reqwest::Client::new();

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

async fn run_review(args: ReviewArgs) -> Result<()> {
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
            )
            .await?;

        println!("{}", batch_id);
    } else {
        // Use responses API
        let result = process_response(
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

async fn run_list_models(args: ListModelsArgs) -> Result<()> {
    // Get API key from args or environment
    let api_key = args
        .api_key
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("OpenAI API key must be provided via --api-key argument or OPENAI_API_KEY environment variable")?;

    let mut models = list_models(&api_key).await?;

    // Sort by id for consistent output
    models.sort_by(|a, b| a.id.cmp(&b.id));

    for model in models {
        println!("{}", model.id);
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Review(args) => run_review(args).await,
        Commands::ListModels(args) => run_list_models(args).await,
    }
}
