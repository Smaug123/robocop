# Plan: Internal State Dashboard for Robocop Server

## Summary

Add a `/dashboard` endpoint to robocop-server that displays Robocop's internal state with a master-detail UI. Replace the existing `/status` endpoint. Add event logging infrastructure to show per-PR timelines.

## Requirements

- **Event logging**: Log timestamped events per PR (webhook received, state transitions, batch submitted, etc.)
- **Master-detail layout**: PR list on left, event timeline on right when PR selected
- **7-day window**: Only show PRs with activity in the last 7 days
- **TRON styling**: Match robocop-dashboard theme (cyan/magenta neon, dark backgrounds, Share Tech Mono + Orbitron fonts)
- **Replace /status**: Remove existing endpoint, dashboard becomes the only state view

---

## Implementation Steps

### Step 1: Database Schema Migration (v4 → v5)

**File**: `robocop-server/src/state_machine/repository/sqlite.rs`

Add `pr_events` table:

```sql
CREATE TABLE pr_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_owner TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    event_data TEXT,  -- JSON for event-specific data
    recorded_at INTEGER NOT NULL  -- Unix timestamp
);

CREATE INDEX idx_pr_events_lookup ON pr_events(repo_owner, repo_name, pr_number, recorded_at DESC);
CREATE INDEX idx_pr_events_recent ON pr_events(recorded_at DESC);
```

Increment `CURRENT_SCHEMA_VERSION` to 5 and add migration logic.

### Step 2: Event Types and Repository Methods

**New file**: `robocop-server/src/dashboard/types.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum DashboardEventType {
    WebhookReceived { action: String, head_sha: String },
    CommandReceived { command: String, user: String },
    StateTransition { from_state: String, to_state: String, trigger: String },
    BatchSubmitted { batch_id: String, model: String },
    BatchCompleted { batch_id: String, has_issues: bool },
    BatchFailed { batch_id: String, reason: String },
    BatchCancelled { batch_id: Option<String>, reason: String },
    CommentPosted { comment_id: u64 },
    CheckRunCreated { check_run_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrEvent {
    pub id: i64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub event_type: DashboardEventType,
    pub recorded_at: i64,
}
```

**File**: `robocop-server/src/state_machine/repository/sqlite.rs`

Add repository methods:
- `log_event(&self, event: &PrEvent) -> Result<(), RepositoryError>`
- `get_pr_events(&self, pr_id: &StateMachinePrId, limit: usize) -> Result<Vec<PrEvent>, RepositoryError>`
- `get_prs_with_recent_activity(&self, since_timestamp: i64) -> Result<Vec<PrSummary>, RepositoryError>`
- `cleanup_old_events(&self, older_than: i64) -> Result<usize, RepositoryError>`

### Step 3: Event Logging Integration

**File**: `robocop-server/src/state_machine/store.rs`

In `process_event()` at line ~596, after `transition()` call:

```rust
let TransitionResult { state, effects } = transition(current_state.clone(), event.clone());

// Log state transition (best-effort, don't fail on logging errors)
if let Err(e) = self.log_state_transition(pr_id, &current_state, &state, &event).await {
    warn!("Failed to log event for PR #{}: {}", pr_id.pr_number, e);
}

current_state = state;
```

Add helper method `log_state_transition()` that creates a `PrEvent` with `DashboardEventType::StateTransition`.

### Step 4: Dashboard API Handlers

**New file**: `robocop-server/src/dashboard/handlers.rs`

Endpoints:
- `GET /dashboard` → Serve embedded HTML (content-negotiation, HTML for browsers)
- `GET /dashboard/api/prs` → JSON list of PRs with recent activity (7-day window)
- `GET /dashboard/api/prs/:owner/:repo/:pr_number/events` → JSON events for specific PR

Auth: Reuse `STATUS_AUTH_TOKEN` env var for API endpoints. HTML page prompts for token and stores in localStorage.

### Step 5: Dashboard HTML/CSS/JS

**New file**: `robocop-server/src/dashboard/dashboard.html`

Embedded via `include_str!()`. Structure:

```
┌─────────────────────────────────────────────────────────────┐
│ ROBOCOP                                    [Refresh] [30s▼] │
├───────────────────────┬─────────────────────────────────────┤
│ PR List (35%)         │ Event Timeline (65%)                │
│                       │                                     │
│ ▸ owner/repo #42      │ ● StateTransition                   │
│   BatchPending        │   Idle → Preparing                  │
│   2m ago              │   12:34:56                          │
│                       │                                     │
│ ▸ owner/repo #41      │ ● BatchSubmitted                    │
│   Completed           │   batch_abc123, gpt-5               │
│   1h ago              │   12:35:02                          │
│                       │                                     │
└───────────────────────┴─────────────────────────────────────┘
```

Styling from `~/Documents/GitHub/robocop-dashboard/styles.css`:
- CSS custom properties: `--cyan: #00d4ff`, `--bg-deep: #050508`, etc.
- Google Fonts: Share Tech Mono, Orbitron
- Glow effects, scan-line animations
- State badges with color coding (green=completed, orange=pending, red=failed)

### Step 6: Route Registration and Cleanup

**File**: `robocop-server/src/main.rs`

- Remove `/status` route
- Add `/dashboard`, `/dashboard/api/prs`, `/dashboard/api/prs/:owner/:repo/:pr_number/events` routes
- Add periodic cleanup job for events older than 7 days (run every hour in batch polling loop)

**File**: `robocop-server/src/lib.rs`

- Add `pub mod dashboard;`
- Remove `pub mod status;` (or keep if other code uses status types)

### Step 7: Update CLAUDE.md

Update documentation to reflect:
- `/status` replaced by `/dashboard`
- New `STATUS_AUTH_TOKEN` usage for dashboard access

---

## Files to Modify

| File | Changes |
|------|---------|
| `robocop-server/src/state_machine/repository/sqlite.rs` | Schema v5 migration, event logging methods |
| `robocop-server/src/state_machine/repository/mod.rs` | Add `PrEvent` re-export, update `StateRepository` trait |
| `robocop-server/src/state_machine/store.rs` | Add event logging in `process_event()` |
| `robocop-server/src/main.rs` | Replace /status with /dashboard routes, add cleanup job |
| `robocop-server/src/lib.rs` | Add `dashboard` module |
| `CLAUDE.md` | Update endpoint documentation |

## New Files

| File | Purpose |
|------|---------|
| `robocop-server/src/dashboard/mod.rs` | Module declaration |
| `robocop-server/src/dashboard/types.rs` | `DashboardEventType`, `PrEvent`, `PrSummary` types |
| `robocop-server/src/dashboard/handlers.rs` | Axum handlers for dashboard endpoints |
| `robocop-server/src/dashboard/dashboard.html` | Embedded HTML/CSS/JS for dashboard UI |

## Files to Delete

| File | Reason |
|------|--------|
| `robocop-server/src/status.rs` | Replaced by dashboard (may keep types if reused) |

---

## API Contract

### GET /dashboard/api/prs

```json
{
  "version": "0.1.0",
  "prs": [
    {
      "repo_owner": "owner",
      "repo_name": "repo",
      "pr_number": 42,
      "current_state": "BatchPending",
      "latest_event_at": 1704825600,
      "event_count": 5,
      "reviews_enabled": true
    }
  ]
}
```

### GET /dashboard/api/prs/:owner/:repo/:pr_number/events

```json
{
  "repo_owner": "owner",
  "repo_name": "repo",
  "pr_number": 42,
  "current_state": "BatchPending",
  "reviews_enabled": true,
  "events": [
    {
      "id": 123,
      "event_type": {
        "type": "StateTransition",
        "data": {
          "from_state": "Idle",
          "to_state": "Preparing",
          "trigger": "PrUpdated"
        }
      },
      "recorded_at": 1704825600
    }
  ]
}
```

---

## Design Decisions

1. **Embedded HTML**: Use `include_str!()` for zero runtime dependencies, single artifact deployment
2. **Polling over WebSocket**: Simpler, matches existing patterns, 30s default refresh is adequate
3. **Best-effort logging**: Event logging failures don't block state machine operation
4. **Reuse STATUS_AUTH_TOKEN**: Single auth mechanism for simplicity
5. **7-day cleanup**: Run hourly to keep database size bounded
