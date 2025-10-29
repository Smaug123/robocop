use serde::{Deserialize, Serialize};

/// Metadata for a code review
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewMetadata {
    pub head_hash: String,
    pub merge_base: String,
    pub branch_name: Option<String>,
    pub repo_name: String,
    pub remote_url: Option<String>,
    pub pull_request_url: Option<String>,
}

impl ReviewMetadata {
    pub fn new(
        head_hash: String,
        merge_base: String,
        branch_name: Option<String>,
        repo_name: String,
        remote_url: Option<String>,
        pull_request_url: Option<String>,
    ) -> Self {
        Self {
            head_hash,
            merge_base,
            branch_name,
            repo_name,
            remote_url,
            pull_request_url,
        }
    }

    /// Create ReviewMetadata from GitData
    pub fn from_git_data(git_data: &crate::GitData, pull_request_url: Option<String>) -> Self {
        Self {
            head_hash: git_data.head_hash.clone(),
            merge_base: git_data.merge_base_hash.clone(),
            branch_name: git_data.branch_name.clone(),
            repo_name: git_data.repo_name.clone(),
            remote_url: git_data.remote_url.clone(),
            pull_request_url,
        }
    }
}

/// System prompt for code review
pub fn get_system_prompt() -> String {
    include_str!("../prompt.txt").to_string()
}

/// Create a user prompt from diff and file contents
pub fn create_user_prompt(
    diff: &str,
    file_contents: &[(String, String)], // (file_path, content) pairs
    additional_prompt: Option<&str>,
) -> String {
    let mut user_prompt = String::from(
        "Below is a git diff, and then the contents of the altered files after the diff was applied.\n",
    );

    if let Some(additional) = additional_prompt {
        user_prompt.push_str(additional);
        user_prompt.push('\n');
    }

    user_prompt.push_str("\nDIFF BEGINS:\n");
    user_prompt.push_str(diff);
    user_prompt.push_str("\nDIFF ENDS\n\nFILE CONTENTS AFTER DIFF APPLIED (omits non-Unicode files and files deleted in the diff):\n\n");

    for (file_path, content) in file_contents {
        user_prompt.push_str(&format!("\n === {} ===\n\n{}\n\n", file_path, content));
    }

    user_prompt
}
