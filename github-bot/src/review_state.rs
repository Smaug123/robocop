use serde::{Deserialize, Serialize};

/// Review state for a pull request
///
/// Using a discriminated union instead of boolean to make the intent explicit
/// and allow for future extension (e.g., TemporarilyDisabled with expiry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    /// Reviews are enabled (default behavior)
    Enabled,
    /// Reviews are explicitly disabled by user request
    Disabled,
}

impl Default for ReviewState {
    fn default() -> Self {
        ReviewState::Enabled
    }
}

/// Unique identifier for a pull request across repositories
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PullRequestId {
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
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
