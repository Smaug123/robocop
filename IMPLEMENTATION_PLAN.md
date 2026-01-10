# Implementation Plan: Internal State Dashboard

This plan decomposes DESIGN.md into reviewable implementation stages, each with a clear correctness oracle.

---

## Stage 1: Database Schema and Event Types

**Dependencies**: None

**Implements**: DESIGN.md §Step 1 (Database Schema Migration) and §Step 2 (Event Types)

### Changes

**File**: `robocop-server/src/state_machine/repository/sqlite.rs`
- Increment `CURRENT_SCHEMA_VERSION` from 4 to 5
- Add migration for `pr_events` table:

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

**New file**: `robocop-server/src/dashboard/mod.rs`
- Module declaration with `pub mod types;`

**New file**: `robocop-server/src/dashboard/types.rs`
- Define `DashboardEventType` enum (tagged union for serde):
  - `WebhookReceived { action: String, head_sha: String }`
  - `CommandReceived { command: String, user: String }`
  - `StateTransition { from_state: String, to_state: String, trigger: String }`
  - `BatchSubmitted { batch_id: String, model: String }`
  - `BatchCompleted { batch_id: String, has_issues: bool }`
  - `BatchFailed { batch_id: String, reason: String }`
  - `BatchCancelled { batch_id: Option<String>, reason: String }`
  - `CommentPosted { comment_id: u64 }`
  - `CheckRunCreated { check_run_id: u64 }`
- Define `PrEvent` struct with `id`, `repo_owner`, `repo_name`, `pr_number`, `event_type`, `recorded_at`
- Define `PrSummary` struct for listing PRs with recent activity

**File**: `robocop-server/src/lib.rs`
- Add `pub mod dashboard;`

### Correctness Oracle

1. **Migration test**: Create a fresh database and verify schema version is 5
2. **Upgrade test**: Create a v4 database, run migrations, verify v5 and `pr_events` table exists
3. **Type serialization test**: Round-trip serialize/deserialize each `DashboardEventType` variant

```rust
#[test]
fn test_dashboard_event_type_serialization_roundtrip() {
    // Property: for all event types, deserialize(serialize(e)) == e
    let events = vec![
        DashboardEventType::WebhookReceived { action: "opened".into(), head_sha: "abc123".into() },
        DashboardEventType::StateTransition { from_state: "Idle".into(), to_state: "Preparing".into(), trigger: "PrUpdated".into() },
        // ... all variants
    ];
    for event in events {
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DashboardEventType = serde_json::from_str(&json).unwrap();
        assert_eq!(event, parsed);
    }
}
```

---

## Stage 2: Repository Methods for Event Logging

**Dependencies**: Stage 1

**Implements**: DESIGN.md §Step 2 (Repository Methods)

### Changes

**File**: `robocop-server/src/state_machine/repository/mod.rs`
- Add to `StateRepository` trait:
  - `async fn log_event(&self, event: &PrEvent) -> Result<(), RepositoryError>`
  - `async fn get_pr_events(&self, pr_id: &StateMachinePrId, limit: usize) -> Result<Vec<PrEvent>, RepositoryError>`
  - `async fn get_prs_with_recent_activity(&self, since_timestamp: i64) -> Result<Vec<PrSummary>, RepositoryError>`
  - `async fn cleanup_old_events(&self, older_than: i64) -> Result<usize, RepositoryError>`
- Re-export `PrEvent`, `PrSummary`, `DashboardEventType` from dashboard types

**File**: `robocop-server/src/state_machine/repository/sqlite.rs`
- Implement the new trait methods using SQLite
- `log_event`: INSERT into `pr_events`
- `get_pr_events`: SELECT with ORDER BY recorded_at DESC, LIMIT
- `get_prs_with_recent_activity`: SELECT DISTINCT (repo_owner, repo_name, pr_number) with JOIN to pr_states for current_state, WHERE recorded_at > since_timestamp
- `cleanup_old_events`: DELETE WHERE recorded_at < older_than, return affected rows

**File**: `robocop-server/src/state_machine/repository/memory.rs`
- Add stub implementations that return empty results (in-memory repo is for testing, doesn't need full event logging)

### Correctness Oracle

Property-based tests using `proptest` or `quickcheck`:

```rust
// Property 1: log_event followed by get_pr_events returns the event
#[test]
fn prop_logged_events_are_retrievable() {
    // For all valid PrEvent e:
    //   log_event(e); get_pr_events(pr_id, 100) contains e
}

// Property 2: get_pr_events returns events in reverse chronological order
#[test]
fn prop_events_ordered_by_time_descending() {
    // For all sequences of events with different timestamps:
    //   get_pr_events returns them sorted by recorded_at DESC
}

// Property 3: cleanup_old_events removes only old events
#[test]
fn prop_cleanup_removes_only_old_events() {
    // For all events with timestamps t1 < cutoff < t2:
    //   after cleanup(cutoff), events with t1 are gone, events with t2 remain
}

// Property 4: get_prs_with_recent_activity returns PRs with events after threshold
#[test]
fn prop_recent_activity_respects_threshold() {
    // For all events with timestamp t and threshold T:
    //   PR appears in result iff t >= T
}

// Property 5: cleanup returns accurate count
#[test]
fn prop_cleanup_returns_count_of_deleted() {
    // For all sets of events:
    //   cleanup(cutoff) returns count == number of events with recorded_at < cutoff
}
```

---

## Stage 3a: Dashboard API Handlers

**Dependencies**: Stage 2

**Implements**: DESIGN.md §Step 4 (Dashboard API Handlers)

### Changes

**New file**: `robocop-server/src/dashboard/handlers.rs`
- `get_dashboard_html`: Return embedded HTML (include_str!)
- `get_prs_api`: Query `get_prs_with_recent_activity(7_days_ago)`, return JSON
- `get_pr_events_api`: Query `get_pr_events(pr_id, 100)`, return JSON with current state
- Auth middleware: Reuse `validate_status_auth` pattern with `STATUS_AUTH_TOKEN`

**File**: `robocop-server/src/dashboard/mod.rs`
- Add `pub mod handlers;`

### API Response Types

```rust
#[derive(Serialize)]
struct PrsApiResponse {
    version: String,
    prs: Vec<PrSummaryResponse>,
}

#[derive(Serialize)]
struct PrSummaryResponse {
    repo_owner: String,
    repo_name: String,
    pr_number: u64,
    current_state: String,
    latest_event_at: i64,
    event_count: usize,
    reviews_enabled: bool,
}

#[derive(Serialize)]
struct PrEventsApiResponse {
    repo_owner: String,
    repo_name: String,
    pr_number: u64,
    current_state: String,
    reviews_enabled: bool,
    events: Vec<PrEventResponse>,
}
```

### Correctness Oracle

Integration tests:

```rust
#[tokio::test]
async fn test_get_prs_api_returns_only_recent() {
    // Setup: Insert events for PR#1 (8 days ago) and PR#2 (3 days ago)
    // Act: GET /dashboard/api/prs
    // Assert: Response contains PR#2 only
}

#[tokio::test]
async fn test_get_pr_events_api_returns_events_in_order() {
    // Setup: Insert 5 events for PR#1 at different times
    // Act: GET /dashboard/api/prs/owner/repo/1/events
    // Assert: Events are in descending order by recorded_at
}

#[tokio::test]
async fn test_api_requires_auth_token() {
    // Act: GET /dashboard/api/prs without Authorization header
    // Assert: 401 Unauthorized
}

#[tokio::test]
async fn test_html_does_not_require_auth() {
    // Act: GET /dashboard with Accept: text/html
    // Assert: 200 OK with HTML content
}
```

---

## Stage 3b: Event Logging Integration

**Dependencies**: Stage 2

**Implements**: DESIGN.md §Step 3 (Event Logging Integration)

### Changes

**File**: `robocop-server/src/state_machine/store.rs`
- Add `repository` field access (it's already `Arc<dyn StateRepository>`)
- In `process_event()` at line ~596, after `transition()` call:

```rust
let TransitionResult { state, effects } = transition(current_state.clone(), event.clone());

// Log state transition (best-effort, don't fail on logging errors)
self.log_state_transition(pr_id, &current_state, &state, &event).await;

current_state = state;
```

- Add helper method:

```rust
async fn log_state_transition(
    &self,
    pr_id: &StateMachinePrId,
    from_state: &ReviewMachineState,
    to_state: &ReviewMachineState,
    event: &Event,
) {
    // Only log if state actually changed
    if std::mem::discriminant(from_state) == std::mem::discriminant(to_state) {
        return;
    }

    let pr_event = PrEvent {
        id: 0,  // Assigned by DB
        repo_owner: pr_id.repo_owner.clone(),
        repo_name: pr_id.repo_name.clone(),
        pr_number: pr_id.pr_number,
        event_type: DashboardEventType::StateTransition {
            from_state: state_name(from_state),
            to_state: state_name(to_state),
            trigger: event.log_summary(),
        },
        recorded_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    };

    if let Err(e) = self.repository.log_event(&pr_event).await {
        warn!("Failed to log event for PR #{}: {}", pr_id.pr_number, e);
    }
}
```

- Add additional event logging points:
  - In webhook handler: `WebhookReceived` event
  - In command handler: `CommandReceived` event
  - After batch submission: `BatchSubmitted` event
  - After batch completion: `BatchCompleted`/`BatchFailed` events

### Correctness Oracle

Integration test with real state machine:

```rust
#[tokio::test]
async fn test_state_transitions_are_logged() {
    // Setup: Create StateStore with SqliteRepository
    let repo = SqliteRepository::new_in_memory().unwrap();
    let store = StateStore::with_repository(Arc::new(repo.clone()));

    // Act: Process PrUpdated event (causes Idle -> Preparing transition)
    let pr_id = StateMachinePrId::new("owner", "repo", 1);
    store.process_event(&pr_id, Event::PrUpdated { ... }, &ctx).await.unwrap();

    // Assert: pr_events contains StateTransition from Idle to Preparing
    let events = repo.get_pr_events(&pr_id, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].event_type {
        DashboardEventType::StateTransition { from_state, to_state, .. } => {
            assert_eq!(from_state, "Idle");
            assert_eq!(to_state, "Preparing");
        }
        _ => panic!("Expected StateTransition event"),
    }
}

#[tokio::test]
async fn test_same_state_not_logged() {
    // Property: If transition doesn't change state variant, no event is logged
    // Setup: Put PR in Idle state, send event that doesn't change state
    // Assert: No new events logged
}
```

---

## Stage 4: Dashboard HTML/CSS/JS

**Dependencies**: Stage 3a

**Implements**: DESIGN.md §Step 5 (Dashboard HTML/CSS/JS)

### Changes

**New file**: `robocop-server/src/dashboard/dashboard.html`
- Self-contained HTML with embedded CSS and JS
- TRON styling (cyan/magenta neon, dark backgrounds)
- Google Fonts: Share Tech Mono, Orbitron
- Master-detail layout:
  - Left panel (35%): PR list with state badges
  - Right panel (65%): Event timeline for selected PR
- Token management:
  - On load, check localStorage for token
  - If missing, show token input prompt
  - Store token in localStorage after validation
- Auto-refresh: 30s default, configurable (30s, 1m, 2m, 5m)
- State badges with color coding:
  - Green: Completed
  - Orange: BatchPending, Preparing, BatchSubmitting
  - Red: Failed
  - Gray: Cancelled, Idle
- XSS-safe: Use textContent for all model outputs

### Correctness Oracle

1. **Manual smoke test**: Open dashboard in browser, verify layout matches design
2. **Token flow test**: Clear localStorage, verify token prompt appears, enter token, verify data loads
3. **XSS test**: Insert event with `<script>alert(1)</script>` in trigger field, verify it renders as text

```rust
#[tokio::test]
async fn test_dashboard_serves_html() {
    // Act: GET /dashboard with Accept: text/html
    // Assert: Content-Type is text/html, body contains expected structure
    let response = client.get("/dashboard").header("Accept", "text/html").send().await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("content-type").unwrap().to_str().unwrap().contains("text/html"));
    let body = response.text().await.unwrap();
    assert!(body.contains("ROBOCOP"));
    assert!(body.contains("Share Tech Mono"));
}

#[tokio::test]
async fn test_dashboard_has_csp_header() {
    // Security: Verify restrictive CSP is set
    let response = client.get("/dashboard").header("Accept", "text/html").send().await;
    let csp = response.headers().get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("script-src 'self' 'unsafe-inline'"));
    assert!(csp.contains("connect-src 'self'"));
}
```

---

## Stage 5: Route Registration and Cleanup

**Dependencies**: Stage 3a, Stage 3b, Stage 4

**Implements**: DESIGN.md §Step 6 (Route Registration and Cleanup)

### Changes

**File**: `robocop-server/src/main.rs`
- Remove `/status` route
- Add routes:
  - `GET /dashboard` → `dashboard::handlers::get_dashboard_html`
  - `GET /dashboard/api/prs` → `dashboard::handlers::get_prs_api`
  - `GET /dashboard/api/prs/:owner/:repo/:pr_number/events` → `dashboard::handlers::get_pr_events_api`
- Update `help_handler` to document `/dashboard` instead of `/status`
- In batch polling loop, add hourly cleanup:

```rust
// In batch_polling_loop, after processing batches:
let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs() as i64;
let seven_days_ago = now - (7 * 24 * 60 * 60);

// Run cleanup every hour (check based on poll count or time)
if should_run_cleanup() {
    let deleted = state.state_store.cleanup_old_events(seven_days_ago).await;
    if deleted > 0 {
        info!("Cleaned up {} old dashboard events", deleted);
    }
}
```

**File**: `robocop-server/src/lib.rs`
- Keep `pub mod status;` if types are reused, otherwise remove

### Correctness Oracle

End-to-end integration test:

```rust
#[tokio::test]
async fn test_full_dashboard_flow() {
    // Setup: Start server with test config
    // Act 1: POST webhook that triggers PR review
    // Assert: PR appears in /dashboard/api/prs
    // Act 2: Wait for batch completion (or mock it)
    // Assert: Events appear in /dashboard/api/prs/owner/repo/1/events
}

#[tokio::test]
async fn test_status_endpoint_removed() {
    // Act: GET /status
    // Assert: 404 Not Found
}

#[tokio::test]
async fn test_old_events_cleaned_up() {
    // Setup: Insert events from 8 days ago and 3 days ago
    // Act: Run cleanup with 7-day threshold
    // Assert: Only 3-day-old events remain
}
```

---

## Stage 6: Documentation Update

**Dependencies**: Stage 5

**Implements**: DESIGN.md §Step 7 (Update CLAUDE.md)

### Changes

**File**: `CLAUDE.md`
- Update Configuration section: Remove `/status` references, add `/dashboard`
- Update endpoint documentation:
  - Remove `/status` endpoint description
  - Add `/dashboard` endpoint description
  - Add `/dashboard/api/prs` and `/dashboard/api/prs/:owner/:repo/:pr_number/events`
- Update `STATUS_AUTH_TOKEN` description to mention it enables dashboard access

### Correctness Oracle

1. **Documentation accuracy**: All mentioned endpoints exist and behave as documented
2. **No stale references**: `grep -r "/status"` in CLAUDE.md returns no matches

---

## Parallel Execution Graph

```
Stage 1
   │
   ▼
Stage 2
   │
   ├──────────────┬──────────────┐
   ▼              ▼              │
Stage 3a      Stage 3b           │
   │              │              │
   ├──────────────┤              │
   ▼              │              │
Stage 4           │              │
   │              │              │
   ├──────────────┴──────────────┘
   ▼
Stage 5
   │
   ▼
Stage 6
```

Stages 3a and 3b can be implemented in parallel since:
- 3a (API handlers) only reads from the database
- 3b (event logging) only writes to the database
- Neither depends on the other's code

Stage 4 depends on 3a (needs handlers to mount HTML endpoint), but not 3b.

---

## Files Summary

### New Files
| File | Stage |
|------|-------|
| `robocop-server/src/dashboard/mod.rs` | 1 |
| `robocop-server/src/dashboard/types.rs` | 1 |
| `robocop-server/src/dashboard/handlers.rs` | 3a |
| `robocop-server/src/dashboard/dashboard.html` | 4 |

### Modified Files
| File | Stages |
|------|--------|
| `robocop-server/src/state_machine/repository/sqlite.rs` | 1, 2 |
| `robocop-server/src/state_machine/repository/mod.rs` | 2 |
| `robocop-server/src/state_machine/repository/memory.rs` | 2 |
| `robocop-server/src/state_machine/store.rs` | 3b |
| `robocop-server/src/main.rs` | 5 |
| `robocop-server/src/lib.rs` | 1, 5 |
| `CLAUDE.md` | 6 |

### Potentially Removed Files
| File | Stage | Condition |
|------|-------|-----------|
| `robocop-server/src/status.rs` | 5 | If types not reused elsewhere |
