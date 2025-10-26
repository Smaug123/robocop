# Implementation Plan: Review Suppression Feature

## Overview

This document outlines the implementation plan for adding review suppression functionality to the robocop GitHub bot.

### Feature Requirements

- Detect `@smaug123-robocop disable-reviews` in PR description to suppress automatic reviews
- Post/update comment saying "Not reviewing commit `<hash>` due to explicit suppression"
- Persist suppression state across pushes to the same PR
- Support `@smaug123-robocop review` command to override suppression (one-time review)
- Support `@smaug123-robocop enable-reviews` / `disable-reviews` commands in PR comments
- Use a two-case discriminated union (not boolean) for state representation
- Rehydrate state on-demand from PR description and comment history

### Out of Scope (For Initial Implementation)

- Rehydrating state on server restart (can be implemented separately later)

## Current Architecture Analysis

### Key Components

**Files:**
- `github-bot/src/webhook.rs:593-820` - `process_code_review()` handles PR review submission
- `github-bot/src/webhook.rs:199-264` - Handles `opened`/`synchronize` PR actions
- `github-bot/src/webhook.rs:266-376` - Handles `created` comment actions
- `github-bot/src/command.rs` - Command parsing (currently: `Review`, `Cancel`)
- `github-bot/src/lib.rs:48-55` - `AppState` definition
- `github-bot/src/github.rs:910-964` - `manage_robocop_comment()` for comment management
- `github-bot/src/main.rs:100-101` - Help endpoint handler

**Current State:**
- `AppState` stores `pending_batches` HashMap for tracking in-flight reviews
- Webhook handles `opened`, `synchronize`, `created` actions
- Commands: `@smaug123-robocop review` (manual review) and `@smaug123-robocop cancel` (cancel pending)
- Help endpoint at `/help` serves JSON/HTML documentation

## Implementation Phases

### Phase 1: Core Types and State Management

**New File:** `github-bot/src/review_state.rs`

Create new module for review state management:

```rust
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
```

**Rationale:** Using a discriminated union (enum) instead of boolean makes the code more self-documenting and allows future extension (e.g., `TemporarilyDisabled` with a timestamp).

**Update:** `github-bot/src/lib.rs`
- Add `pub mod review_state;` to module declarations
- Import types: `pub use review_state::{ReviewState, PullRequestId};`
- Add to `AppState` struct: `pub review_states: Arc<RwLock<HashMap<PullRequestId, ReviewState>>>`

**Update:** `github-bot/src/main.rs`
- Initialize `review_states: Arc::new(RwLock::new(HashMap::new()))` when creating `AppState`

### Phase 2: GitHub API Extensions

**Update:** `github-bot/src/github.rs`

Add PR body field to existing `PullRequestResponse` struct (around line 122):

```rust
#[derive(Debug, Deserialize)]
pub struct PullRequestResponse {
    pub number: u64,
    pub head: PullRequestRefResponse,
    pub base: PullRequestRefResponse,
    pub body: Option<String>,  // ADD THIS FIELD
}
```

Note: This field already exists in GitHub's API response; we just need to deserialize it.

Add helper function to check for suppression marker:

```rust
/// Check if a PR description or comment contains the disable-reviews marker
///
/// This performs a case-insensitive search for the @smaug123-robocop disable-reviews
/// pattern anywhere in the text (not just at line start, since PR descriptions
/// are not constrained like comments).
pub fn contains_disable_reviews_marker(text: &str) -> bool {
    for line in text.lines() {
        if line.trim().to_lowercase().contains("@smaug123-robocop disable-reviews") {
            return true;
        }
    }
    false
}
```

**Rationale:** PR descriptions are free-form text, so we search for the marker anywhere in the text rather than requiring it at line start like comment commands.

### Phase 3: Command Parser Extensions

**Update:** `github-bot/src/command.rs`

Extend `RobocopCommand` enum (around line 6):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobocopCommand {
    /// Request a code review
    Review,
    /// Cancel all pending reviews for this PR
    Cancel,
    /// Enable automatic reviews
    EnableReviews,
    /// Disable automatic reviews
    DisableReviews,
}
```

Update `Display` implementation:

```rust
impl fmt::Display for RobocopCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RobocopCommand::Review => write!(f, "review"),
            RobocopCommand::Cancel => write!(f, "cancel"),
            RobocopCommand::EnableReviews => write!(f, "enable-reviews"),
            RobocopCommand::DisableReviews => write!(f, "disable-reviews"),
        }
    }
}
```

Update `parse_comment()` function to handle new commands (around line 50):

```rust
// Parse the command
if command_part.eq_ignore_ascii_case("review") {
    return Some(RobocopCommand::Review);
} else if command_part.eq_ignore_ascii_case("cancel") {
    return Some(RobocopCommand::Cancel);
} else if command_part.eq_ignore_ascii_case("enable-reviews") {
    return Some(RobocopCommand::EnableReviews);
} else if command_part.eq_ignore_ascii_case("disable-reviews") {
    return Some(RobocopCommand::DisableReviews);
}
```

Add comprehensive tests:

```rust
#[test]
fn test_parse_enable_reviews_command() {
    assert_eq!(
        parse_comment("@smaug123-robocop enable-reviews"),
        Some(RobocopCommand::EnableReviews)
    );
    assert_eq!(
        parse_comment("@smaug123-robocop Enable-Reviews"),
        Some(RobocopCommand::EnableReviews)
    );
    assert_eq!(
        parse_comment("  @smaug123-robocop ENABLE-REVIEWS  "),
        Some(RobocopCommand::EnableReviews)
    );
}

#[test]
fn test_parse_disable_reviews_command() {
    assert_eq!(
        parse_comment("@smaug123-robocop disable-reviews"),
        Some(RobocopCommand::DisableReviews)
    );
    assert_eq!(
        parse_comment("@smaug123-robocop Disable-Reviews"),
        Some(RobocopCommand::DisableReviews)
    );
}

#[test]
fn test_enable_disable_multiline() {
    let comment = "Please review this.\n\n@smaug123-robocop enable-reviews";
    assert_eq!(parse_comment(comment), Some(RobocopCommand::EnableReviews));
}
```

### Phase 4: State Rehydration Logic

**New functions in:** `github-bot/src/review_state.rs`

Implement on-demand state rehydration:

```rust
use crate::github::GitHubClient;
use crate::command::{parse_comment, RobocopCommand};
use anyhow::Result;
use tracing::info;

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
        .get_pull_request(correlation_id, installation_id, repo_owner, repo_name, pr_number)
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
        .get_pr_comments(correlation_id, installation_id, repo_owner, repo_name, pr_number)
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

/// Helper function from github.rs - checks if text contains disable marker
fn contains_disable_reviews_marker(text: &str) -> bool {
    for line in text.lines() {
        if line.trim().to_lowercase().contains("@smaug123-robocop disable-reviews") {
            return true;
        }
    }
    false
}
```

**Rationale:** State is rehydrated on-demand rather than at server startup. This keeps startup fast and ensures we always have the most current state from GitHub's source of truth.

### Phase 5: Webhook Handler Updates

**Update:** `github-bot/src/webhook.rs`

#### 5.1: Handle PR Description Changes

Update the webhook action handler to include `edited` (around line 199):

```rust
match payload.action.as_deref() {
    Some("opened") | Some("synchronize") | Some("edited") => {
        info!("Processing PR event: {:?}", payload.action);
        // ... existing logic ...
```

**Rationale:** When a PR description is edited, we need to check if the disable-reviews marker was added or removed.

#### 5.2: Update `process_code_review()` Function

Add `force_review` parameter and state checking logic at the beginning of the function (around line 593):

```rust
async fn process_code_review(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr: PullRequest,
    force_review: bool,  // ADD THIS PARAMETER
) -> anyhow::Result<()> {
    info!(
        "Processing code review for PR #{} in {} (force: {})",
        pr.number, repo.full_name, force_review
    );

    let github_client = &state.github_client;
    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;

    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number: pr.number,
    };

    // Get or rehydrate review state
    let review_state = {
        let states = state.review_states.read().await;
        if let Some(&cached_state) = states.get(&pr_id) {
            cached_state
        } else {
            drop(states); // Release read lock before expensive operation

            // Rehydrate state from PR history
            let rehydrated_state = crate::review_state::rehydrate_review_state(
                github_client,
                correlation_id,
                installation_id,
                repo_owner,
                repo_name,
                pr.number,
            ).await?;

            // Store it for future use
            let mut states = state.review_states.write().await;
            states.insert(pr_id.clone(), rehydrated_state);
            rehydrated_state
        }
    };

    // Check if reviews are suppressed (unless forced by explicit command)
    if review_state == crate::ReviewState::Disabled && !force_review {
        info!(
            "Reviews are disabled for PR #{}, posting suppression notice",
            pr.number
        );

        let version = crate::get_bot_version();
        let suppression_content = format!(
            "ℹ️ **Review suppressed**\n\n\
            Not reviewing commit `{}` due to explicit suppression.\n\n\
            To enable reviews, comment `@smaug123-robocop enable-reviews` or \
            request a one-time review with `@smaug123-robocop review`.",
            pr.head.sha
        );

        let pr_info = PullRequestInfo {
            installation_id,
            repo_owner: repo_owner.to_string(),
            repo_name: repo_name.to_string(),
            pr_number: pr.number,
        };

        github_client
            .manage_robocop_comment(correlation_id, &pr_info, &suppression_content, &version)
            .await?;

        return Ok(());
    }

    // ... rest of existing review logic (unchanged) ...
```

**Rationale:** The `force_review` parameter allows `@smaug123-robocop review` to override suppression for a one-time review without changing the persistent state.

#### 5.3: Update All Calls to `process_code_review()`

**Normal PR processing** (around line 230):
```rust
process_code_review(
    correlation_id_clone.as_deref(),
    state_clone,
    installation_id,
    repo,
    pr_clone,
    false,  // force_review: false for automatic triggers
)
.await
```

**Manual review from comment** (around line 590):
```rust
// Use the existing code review logic with force flag
process_code_review(correlation_id, state, installation_id, repo, pr, true).await
```

#### 5.4: Add Handlers for Enable/Disable Commands

In the comment webhook handler (around line 295), add new match arms:

```rust
match robocop_command {
    command::RobocopCommand::Review => {
        // ... existing review logic ...
    }
    command::RobocopCommand::Cancel => {
        // ... existing cancel logic ...
    }
    command::RobocopCommand::EnableReviews => {
        info!("Processing @smaug123-robocop enable-reviews command");

        if let (Some(repo), Some(installation)) =
            (payload.repository.clone(), &payload.installation)
        {
            let state_clone = state.clone();
            let issue_number = issue.number;
            let installation_id = installation.id;
            let correlation_id_clone = correlation_id.clone();

            tokio::spawn(async move {
                info!(
                    "Spawned background task to enable reviews for PR #{}",
                    issue_number
                );

                if let Err(e) = process_enable_reviews(
                    correlation_id_clone.as_deref(),
                    state_clone,
                    installation_id,
                    repo,
                    issue_number,
                ).await {
                    error!("Failed to process enable-reviews: {}", e);
                }
            });
        } else {
            warn!("Missing repository or installation info for enable-reviews command");
        }
    }
    command::RobocopCommand::DisableReviews => {
        info!("Processing @smaug123-robocop disable-reviews command");

        if let (Some(repo), Some(installation)) =
            (payload.repository.clone(), &payload.installation)
        {
            let state_clone = state.clone();
            let issue_number = issue.number;
            let installation_id = installation.id;
            let correlation_id_clone = correlation_id.clone();

            tokio::spawn(async move {
                info!(
                    "Spawned background task to disable reviews for PR #{}",
                    issue_number
                );

                if let Err(e) = process_disable_reviews(
                    correlation_id_clone.as_deref(),
                    state_clone,
                    installation_id,
                    repo,
                    issue_number,
                ).await {
                    error!("Failed to process disable-reviews: {}", e);
                }
            });
        } else {
            warn!("Missing repository or installation info for disable-reviews command");
        }
    }
}
```

#### 5.5: Implement Command Handler Functions

Add these new async functions in `webhook.rs`:

```rust
async fn process_enable_reviews(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Enabling reviews for PR #{} in {}",
        pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };

    // Update state
    {
        let mut states = state.review_states.write().await;
        states.insert(pr_id, crate::ReviewState::Enabled);
    }

    // Fetch PR details to get current commit
    let pr_details = state.github_client
        .get_pull_request(
            correlation_id,
            installation_id,
            repo_owner,
            repo_name,
            pr_number,
        )
        .await?;

    // Post acknowledgment comment with current commit info
    let version = crate::get_bot_version();
    let content = format!(
        "✅ **Reviews enabled**\n\n\
        Automatic reviews have been enabled for this PR.\n\n\
        Submitting review for current commit `{}`...",
        pr_details.head.sha
    );

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state.github_client
        .manage_robocop_comment(correlation_id, &pr_info, &content, &version)
        .await?;

    // Trigger a review for the current commit
    let pr = PullRequest {
        number: pr_details.number,
        head: PullRequestRef {
            sha: pr_details.head.sha,
            ref_name: pr_details.head.ref_name,
        },
        base: PullRequestRef {
            sha: pr_details.base.sha,
            ref_name: pr_details.base.ref_name,
        },
    };

    // Use force_review=false since we just enabled reviews
    process_code_review(correlation_id, state, installation_id, repo, pr, false).await
}

async fn process_disable_reviews(
    correlation_id: Option<&str>,
    state: Arc<AppState>,
    installation_id: u64,
    repo: Repository,
    pr_number: u64,
) -> anyhow::Result<()> {
    info!(
        "Disabling reviews for PR #{} in {}",
        pr_number, repo.full_name
    );

    let repo_owner = &repo.owner.login;
    let repo_name = &repo.name;
    let pr_id = crate::PullRequestId {
        repo_owner: repo_owner.clone(),
        repo_name: repo_name.clone(),
        pr_number,
    };

    // Update state
    {
        let mut states = state.review_states.write().await;
        states.insert(pr_id, crate::ReviewState::Disabled);
    }

    // Cancel any pending reviews for this PR
    let batches_to_cancel: Vec<(String, crate::PendingBatch)> = {
        let pending = state.pending_batches.read().await;
        pending
            .iter()
            .filter(|(_, batch)| {
                batch.pr_number == pr_number
                    && batch.repo_owner == *repo_owner
                    && batch.repo_name == *repo_name
            })
            .map(|(id, batch)| (id.clone(), batch.clone()))
            .collect()
    };

    let cancelled_count = batches_to_cancel.len();

    for (batch_id, _) in &batches_to_cancel {
        info!("Cancelling batch {} due to disable-reviews", batch_id);
        if let Err(e) = state.openai_client.cancel_batch(correlation_id, batch_id).await {
            warn!("Failed to cancel batch {}: {}", batch_id, e);
        }
    }

    // Remove from tracking
    {
        let mut pending = state.pending_batches.write().await;
        for (batch_id, _) in &batches_to_cancel {
            pending.remove(batch_id);
        }
    }

    // Post acknowledgment
    let version = crate::get_bot_version();
    let content = if cancelled_count > 0 {
        format!(
            "🔕 **Reviews disabled**\n\n\
            Automatic reviews have been disabled for this PR.\n\n\
            Cancelled {} pending review{}.",
            cancelled_count,
            if cancelled_count == 1 { "" } else { "s" }
        )
    } else {
        "🔕 **Reviews disabled**\n\n\
        Automatic reviews have been disabled for this PR.".to_string()
    };

    let pr_info = PullRequestInfo {
        installation_id,
        repo_owner: repo_owner.to_string(),
        repo_name: repo_name.to_string(),
        pr_number,
    };

    state.github_client
        .manage_robocop_comment(correlation_id, &pr_info, &content, &version)
        .await?;

    Ok(())
}
```

**Rationale:** `enable-reviews` triggers an immediate review for the current commit, while `disable-reviews` cancels pending reviews to avoid wasting API credits.

### Phase 6: Help Endpoint Updates

**Update:** `github-bot/src/main.rs`

Update the JSON response in `help_handler()` (around line 48):

```rust
"features": [
    "Automated code reviews on PR open/synchronize events",
    "OpenAI batch API integration for cost-effective processing",
    "Superseded commit cancellation using git ancestry",
    "Review status tracking and updates via PR comments",
    "Review suppression via PR description or commands",
    "Manual review trigger via @smaug123-robocop review comment",
    "Enable/disable reviews via @smaug123-robocop enable-reviews/disable-reviews",
    "Cancel pending reviews via @smaug123-robocop cancel comment"
],
```

**Update:** `github-bot/src/help.html`

Add new section documenting the review suppression feature:

```html
<section>
    <h2>Review Suppression</h2>
    <p>You can control whether robocop automatically reviews your pull requests:</p>

    <h3>Disable Reviews via PR Description</h3>
    <p>Include <code>@smaug123-robocop disable-reviews</code> anywhere in your PR description to suppress automatic reviews.</p>

    <h3>Commands</h3>
    <ul>
        <li><code>@smaug123-robocop disable-reviews</code> - Disable automatic reviews and cancel pending reviews</li>
        <li><code>@smaug123-robocop enable-reviews</code> - Enable automatic reviews and trigger review for current commit</li>
        <li><code>@smaug123-robocop review</code> - Request a one-time review even if reviews are disabled</li>
    </ul>

    <p>State persists across commits: once disabled, reviews stay disabled until explicitly enabled.</p>
</section>
```

### Phase 7: Testing Strategy

#### Unit Tests

**`github-bot/src/review_state.rs`:**

```rust
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
```

**`github-bot/src/github.rs`:**

```rust
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
```

**`github-bot/src/command.rs`:**

Already covered in Phase 3 - add tests for `enable-reviews` and `disable-reviews` parsing.

#### Integration Tests

Create `github-bot/tests/review_suppression_test.rs`:

```rust
// Note: These would be integration tests requiring a mock GitHub API
// For now, document the test scenarios that should be verified manually:
//
// 1. PR opened with "@smaug123-robocop disable-reviews" in description
//    - Should post suppression notice instead of review
//    - Subsequent pushes should continue showing suppression notice
//
// 2. Comment "@smaug123-robocop enable-reviews" on suppressed PR
//    - Should update state to Enabled
//    - Should trigger review for current commit
//
// 3. Comment "@smaug123-robocop disable-reviews" on active PR
//    - Should update state to Disabled
//    - Should cancel pending reviews
//    - Subsequent pushes should show suppression notice
//
// 4. Comment "@smaug123-robocop review" on suppressed PR
//    - Should perform one-time review
//    - Should NOT change persistent state
//    - Next push should still be suppressed
//
// 5. PR edited to add/remove "@smaug123-robocop disable-reviews" marker
//    - Should update state accordingly
//
// 6. State rehydration after cache miss
//    - Create PR with marker, comment enable-reviews, verify state is Enabled
//    - Clear in-memory cache, trigger webhook, verify state rehydrated correctly
```

### Phase 8: Documentation Updates

**Update:** `github-bot/prompt.txt`

Add documentation for new commands and behavior:

```
# Review Suppression

Robocop supports suppressing automatic code reviews on a per-PR basis.

## Disabling Reviews

To disable automatic reviews for a pull request:

1. Include `@smaug123-robocop disable-reviews` anywhere in the PR description when creating it
2. Or comment `@smaug123-robocop disable-reviews` on an existing PR

When reviews are disabled:
- New commits will NOT trigger automatic reviews
- A comment will be posted: "Not reviewing commit <hash> due to explicit suppression"
- State persists across commits until explicitly enabled

## Enabling Reviews

To enable automatic reviews:

Comment `@smaug123-robocop enable-reviews` on the PR. This will:
- Enable automatic reviews for future commits
- Immediately trigger a review for the current commit

## One-Time Review Override

To request a single review without changing the persistent state:

Comment `@smaug123-robocop review` on the PR. This works even if reviews are disabled.

## State Persistence

The enable/disable state is stored per-PR and persists across:
- New commits/pushes
- PR description edits
- Server restarts (state is rehydrated from GitHub)

The state is determined by:
1. The PR description's disable-reviews marker (if present)
2. Chronologically applying all enable-reviews/disable-reviews commands from comments
```

**Update:** `CLAUDE.md` (Project Instructions)

Add to the "Architecture Overview" section:

```markdown
### Review State Management

The bot supports per-PR review suppression through:
- PR description markers (`@smaug123-robocop disable-reviews`)
- Comment commands (`enable-reviews`, `disable-reviews`)
- On-demand state rehydration from GitHub API

State is stored in `AppState.review_states` as a `HashMap<PullRequestId, ReviewState>`.
When state is not in memory, it's rehydrated by:
1. Checking PR description for disable marker
2. Chronologically applying enable/disable commands from comment history
```

## Implementation Order (Following TDD)

Per CLAUDE.md guidelines, implement in this order:

1. **Phase 1 - Core Types** (with tests)
   - Create `review_state.rs` with unit tests
   - Update `lib.rs` and `main.rs`

2. **Phase 3 - Command Parsing** (tests first)
   - Write failing tests for new commands
   - Implement command parsing
   - Verify tests pass

3. **Phase 2 - GitHub API Extensions** (with tests)
   - Add `body` field to `PullRequestResponse`
   - Implement `contains_disable_reviews_marker()` with tests

4. **Phase 4 - State Rehydration** (integration tests)
   - Implement `rehydrate_review_state()`
   - Manual testing against real GitHub API

5. **Phase 5 - Webhook Integration**
   - Update `process_code_review()` with state checking
   - Implement command handlers
   - Add webhook action handlers
   - Update all call sites

6. **Phase 6 - Documentation**
   - Update help endpoint
   - Update prompt.txt
   - Update CLAUDE.md

7. **Phase 7 - End-to-End Testing**
   - Manual testing with real PRs
   - Verify all scenarios from integration test list

## Edge Cases and Considerations

### Race Conditions

**Scenario:** Comment command arrives while review is being submitted
- **Solution:** State check happens atomically at the start of `process_code_review()`
- **Impact:** Review may still be submitted if state check passed before command processed

**Scenario:** Multiple commands posted rapidly (e.g., disable, enable, disable)
- **Solution:** Each command updates state independently; last one wins
- **Impact:** State transitions are atomic via RwLock

### State Synchronization

**Scenario:** User manually deletes comments containing state commands
- **Solution:** State remains in memory until server restart or cache eviction
- **Impact:** Minimal - state rehydration on cache miss will reflect current comment state

**Scenario:** PR description edited to add/remove disable marker
- **Solution:** Handle `pull_request.edited` webhook event
- **Impact:** State should be invalidated and rehydrated on next event

### Performance

**Scenario:** Large number of PRs with state
- **Solution:** State stored in-memory HashMap, O(1) lookup
- **Impact:** Memory usage grows with number of active PRs

**Scenario:** Rehydration requires fetching all comments
- **Solution:** Only happens on cache miss; results are cached
- **Impact:** First access after cache miss has higher latency

### API Limits

**Scenario:** Rehydration on PR with hundreds of comments
- **Solution:** GitHub API pagination (already implemented in `get_pr_comments`)
- **Impact:** May require multiple API calls; uses installation token quota

## Future Enhancements (Out of Scope)

These are explicitly not part of the initial implementation:

1. **Server Restart State Rehydration**
   - Current: State rehydrated on-demand when accessed
   - Future: Could proactively rehydrate all active PRs on startup
   - Benefit: Faster first access, but slower startup

2. **Temporary Suppression**
   - Current: Binary Enabled/Disabled state
   - Future: `TemporarilyDisabled { until: DateTime }` enum variant
   - Use case: "Don't review for the next hour while I iterate"

3. **Repository-Level Defaults**
   - Current: All PRs default to Enabled
   - Future: Repository-level configuration for default state
   - Use case: Opt-in review mode for specific repos

4. **State Persistence to Disk**
   - Current: In-memory only, rehydrated from GitHub
   - Future: Persist to disk/database for faster startup
   - Trade-off: Adds complexity, GitHub is already source of truth

## Summary

This feature adds comprehensive review suppression control while maintaining the existing architecture patterns. The key design decisions are:

- **Discriminated union over boolean**: Makes intent explicit and allows future extension
- **On-demand rehydration**: Balances performance with correctness
- **GitHub as source of truth**: No external state storage needed
- **Force parameter for overrides**: Clean separation between persistent state and one-time actions
- **Atomic state updates**: RwLock prevents race conditions

The implementation follows TDD principles and integrates cleanly with the existing webhook, command, and comment management systems.
