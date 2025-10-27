use crate::command::{parse_comment, RobocopCommand};
use crate::github::{contains_disable_reviews_marker, GitHubClient};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Review state for a pull request
///
/// Using a discriminated union instead of boolean to make the intent explicit
/// and allow for future extension (e.g., TemporarilyDisabled with expiry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReviewState {
    /// Reviews are enabled (default behavior)
    #[default]
    Enabled,
    /// Reviews are explicitly disabled by user request
    Disabled,
}

/// Unique identifier for a pull request across repositories
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PullRequestId {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
}

/// Determine the current review state by examining PR description and comment history
///
/// This function is called on-demand when we need the review state for a PR that
/// isn't currently in our in-memory cache. It reconstructs the state by:
/// 1. Checking the PR description for the disable-reviews marker
/// 2. Applying all enable-reviews/disable-reviews commands chronologically
///
/// The last command wins (chronologically by comment creation time).
pub async fn rehydrate_review_state(
    github_client: &GitHubClient,
    correlation_id: Option<&str>,
    installation_id: u64,
    repo_owner: &str,
    repo_name: &str,
    pr_number: u64,
) -> Result<ReviewState> {
    info!(
        "Rehydrating review state for PR #{} in {}/{}",
        pr_number, repo_owner, repo_name
    );

    // 1. Fetch PR details to get description
    let pr = github_client
        .get_pull_request(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            pr_number,
        )
        .await?;

    // 2. Check if PR description contains disable marker
    let mut state = if let Some(body) = &pr.body {
        if contains_disable_reviews_marker(body) {
            info!("PR description contains disable-reviews marker");
            ReviewState::Disabled
        } else {
            ReviewState::Enabled
        }
    } else {
        ReviewState::Enabled
    };

    // 3. Fetch all comments and apply them chronologically
    let comments = github_client
        .get_pr_comments(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            pr_number,
        )
        .await?;

    // Comments are already sorted by creation time (oldest first) from GitHub API
    for comment in comments {
        if let Some(command) = parse_comment(&comment.body) {
            match command {
                RobocopCommand::EnableReviews => {
                    info!("Found enable-reviews command in comment {}", comment.id);
                    state = ReviewState::Enabled;
                }
                RobocopCommand::DisableReviews => {
                    info!("Found disable-reviews command in comment {}", comment.id);
                    state = ReviewState::Disabled;
                }
                _ => {} // Other commands don't affect review state
            }
        }
    }

    info!("Rehydrated state for PR #{}: {:?}", pr_number, state);
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_state_default() {
        assert_eq!(ReviewState::default(), ReviewState::Enabled);
    }

    #[test]
    fn test_pull_request_id_equality() {
        let id1 = PullRequestId {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
        };
        let id2 = PullRequestId {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
        };
        let id3 = PullRequestId {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 456,
        };

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_pull_request_id_hash() {
        use std::collections::HashMap;

        let id = PullRequestId {
            repo_owner: "owner".to_string(),
            repo_name: "repo".to_string(),
            pr_number: 123,
        };

        let mut map = HashMap::new();
        map.insert(id.clone(), ReviewState::Disabled);

        assert_eq!(map.get(&id), Some(&ReviewState::Disabled));
    }
}
