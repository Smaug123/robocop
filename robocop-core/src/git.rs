use serde::{Deserialize, Serialize};

/// Git repository data needed for code review
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitData {
    /// The diff content
    pub diff: String,
    /// List of changed file paths
    pub files_changed: Vec<String>,
    /// HEAD commit hash
    pub head_hash: String,
    /// Merge base commit hash
    pub merge_base_hash: String,
    /// Branch name (if available)
    pub branch_name: Option<String>,
    /// Repository name
    pub repo_name: String,
    /// Remote URL (if available)
    pub remote_url: Option<String>,
}

impl GitData {
    pub fn new(
        diff: String,
        files_changed: Vec<String>,
        head_hash: String,
        merge_base_hash: String,
        branch_name: Option<String>,
        repo_name: String,
        remote_url: Option<String>,
    ) -> Self {
        Self {
            diff,
            files_changed,
            head_hash,
            merge_base_hash,
            branch_name,
            repo_name,
            remote_url,
        }
    }
}
