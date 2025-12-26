use crate::github::{CompareCommitsRequest, GitHubClient};
use anyhow::{anyhow, Result};
use tracing::{info, warn};

pub struct GitOps;

impl GitOps {
    /// Check if the existing_sha is an ancestor of new_sha using GitHub Compare API
    /// This indicates that new_sha supersedes existing_sha in the commit history
    pub async fn is_ancestor(
        github_client: &GitHubClient,
        installation_id: u64,
        repo_owner: &str,
        repo_name: &str,
        new_sha: &str,
        existing_sha: &str,
    ) -> Result<bool> {
        // Validate SHA format (basic check for hex string of reasonable length)
        if !Self::is_valid_sha(new_sha) || !Self::is_valid_sha(existing_sha) {
            return Err(anyhow!(
                "Invalid SHA format: new_sha='{}', existing_sha='{}'",
                new_sha,
                existing_sha
            ));
        }

        info!(
            "Checking if {} is ancestor of {} in {}/{}",
            existing_sha, new_sha, repo_owner, repo_name
        );

        // If the SHAs are the same, they're the same commit (not ancestry)
        if existing_sha == new_sha {
            return Ok(false);
        }

        // Use GitHub Compare API to check if existing_sha is in new_sha's history
        // Compare existing_sha...new_sha - if ahead_by > 0 and behind_by == 0, then existing_sha is an ancestor
        let compare_request = CompareCommitsRequest {
            installation_id,
            repo_owner,
            repo_name,
            base_sha: existing_sha,
            head_sha: new_sha,
        };

        match github_client.compare_commits(None, &compare_request).await {
            Ok(compare_result) => {
                // If ahead_by > 0 and behind_by == 0, it means existing_sha is an ancestor of new_sha
                let is_ancestor = compare_result.ahead_by > 0 && compare_result.behind_by == 0;

                info!(
                    "Ancestry check result: {} is {} an ancestor of {} (behind_by: {}, ahead_by: {})",
                    existing_sha,
                    if is_ancestor { "" } else { "NOT" },
                    new_sha,
                    compare_result.behind_by,
                    compare_result.ahead_by
                );

                Ok(is_ancestor)
            }
            Err(e) => {
                warn!(
                    "Failed to compare commits {} and {}: {}",
                    existing_sha, new_sha, e
                );
                // If comparison fails, be conservative and treat as non-ancestor
                Ok(false)
            }
        }
    }

    /// Basic validation of SHA format
    fn is_valid_sha(sha: &str) -> bool {
        // Git SHAs are hexadecimal and typically 7-40 characters long
        sha.len() >= 7 && sha.len() <= 40 && sha.chars().all(|c| c.is_ascii_hexdigit())
    }
}
