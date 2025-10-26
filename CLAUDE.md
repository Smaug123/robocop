# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This repository contains three main components:

1. **GitHub Bot** (`github-bot/`): A Rust-based webhook server for automated code reviews
2. **Standalone Python Script** (`python/`): A Python CLI tool for automated code reviews
3. **Review Dashboard** (`dashboard.html`): A single HTML file to view the state of all code reviews performed using the tools in the repository

## Development Commands

This project uses Nix for reproducible development environments.

### GitHub Bot (Rust)

The Rust crate is located in the `github-bot/` subdirectory. All commands should be run from the project root within the Nix environment:

- **Build**: `nix develop --command cargo build --manifest-path github-bot/Cargo.toml`
- **Test**: `nix develop --command cargo test --manifest-path github-bot/Cargo.toml`
- **Test with snapshots**: `nix develop --command cargo test --manifest-path github-bot/Cargo.toml --workspace`
- **Lint**: `nix develop --command cargo clippy --manifest-path github-bot/Cargo.toml --all-targets --all-features -- -D warnings`
- **Format**: `nix develop --command cargo fmt --manifest-path github-bot/Cargo.toml`
- **Format Check**: `nix develop --command cargo fmt --manifest-path github-bot/Cargo.toml --check`
- **Run**: `nix develop --command cargo run --manifest-path github-bot/Cargo.toml`
- **Build Nix Package**: `nix build`

Before committing in the Rust subdirectory, always run Clippy and the formatter.

### Standalone Python Script (Python)

#### Environment Setup
```bash
cd python/
make venv          # Create virtual environment and install dependencies
```

#### Code Quality
```bash
cd python/
make lint          # Run ruff check, ruff format --check, and pyright
make format        # Auto-fix ruff issues and format code
```

Run these before declaring a change to be complete.

## Architecture Overview

### GitHub Bot

This is a GitHub webhook-based bot that provides automated code reviews using OpenAI's batch API. The system uses async processing with background polling for batch completion.

### Core Flow
1. **Webhook Reception**: Receives GitHub PR webhooks on `/webhook` endpoint
2. **Event Filtering**: Only processes events from the configured target user ID
3. **Diff Collection**: Gets diffs and changed file contents via GitHub API
4. **Batch Submission**: Submits review request to OpenAI batch API for async processing
5. **Polling Loop**: Background task polls batch status every 60 seconds
6. **Result Processing**: Updates PR comments with review results when batches complete

### Key Components

- **AppState**: Central state container with GitHub/OpenAI clients and pending batch tracking
- **Webhook Handler** (`src/webhook.rs`): Processes GitHub webhooks with signature verification
- **GitHub Client** (`src/github.rs`): Handles GitHub API interactions with JWT authentication
- **OpenAI Client** (`src/openai.rs`): Manages OpenAI batch API requests and polling
- **Recording System** (`src/recording/`): Optional HTTP request/response logging for debugging
- **Git Operations** (`src/git.rs`): Git ancestry checking for superseded commit handling

### Batch Processing
The system uses OpenAI's batch API for cost-effective async processing. When new commits supersede existing ones, it automatically cancels obsolete batches using git ancestry checks.

### Configuration

Environment variables required:
- `GITHUB_APP_ID`: GitHub App ID
- `GITHUB_PRIVATE_KEY`: GitHub App private key (PEM format, `\n` will be converted to newlines)
- `GITHUB_WEBHOOK_SECRET`: Secret for webhook signature verification
- `OPENAI_API_KEY`: OpenAI API key
- `TARGET_GITHUB_USER_ID`: Numeric GitHub user ID to monitor for pushes
- `PORT`: Server port (default: 3000)
- `RECORDING_ENABLED`: Enable HTTP recording (default: false)
- `RECORDING_LOG_PATH`: Recording log file path (default: recordings.jsonl)

### Standalone Python Script

The Python script performs the same review as the GitHub bot (analyzing changes against a merge-base and providing structured feedback on potential issues), but is invoked from the command line.

#### Architecture

- **Core Script**: `python/robocop.py` - Single Python file containing all review functionality.
  - Uses git commands to extract diffs and file changes relative to merge-base
  - Supports both regular chat completions and batch processing APIs
  - Returns JSON-formatted reviews (guaranteed by the Structured Outputs API) with reasoning, substantiveComments, and summary

### Review Dashboard

The `dashboard.html` file provides a web-based interface for monitoring batch review jobs:

- **Batch Monitoring**: View status, creation time, and metadata for all robocop batch jobs
- **Real-time Updates**: Auto-refresh functionality with configurable intervals (30s, 1m, 2m, 5m)
- **Review Results**: Display completed reviews with reasoning, summary, and substantive comments

**Usage**: Open `dashboard.html` in a web browser and enter your OpenAI API key to start monitoring batch reviews. When the Python script was invoked using the `--batch` flag, and when the GitHub bot was used, the script outputs only a batch ID - use the dashboard to monitor progress and retrieve results.

## Tool Usage

### Robocop

#### Basic Review
```bash
cd python/
.venv/bin/python robocop.py --api-key YOUR_API_KEY
```

#### Advanced Options
```bash
cd python/
.venv/bin/python robocop.py \
  --api-key YOUR_API_KEY \
  --default-branch main \
  --reasoning-effort high \
  --additional-prompt "Focus on security issues" \
  --include-files config.py utils.py \
  --batch  # Use batch API for processing; it's cheaper (prints batch ID - use dashboard to view results)
```

#### Dry Run (Preview prompts)
```bash
cd python/
.venv/bin/python robocop.py --api-key fake --dry-run
```

## Key Implementation Details

### GitHub Bot
- **Git Operations**: Git ancestry checking for superseded commit handling
- **Batch Processing**: Uses OpenAI's batch API for cost-effective async processing
- **HTTP Recording**: Optional request/response logging for debugging

### Standalone Python Script
- **Git Operations**: All git commands use `--no-ext-diff` flag to ensure consistent diff output
- **File Reading**: Gracefully handles non-existent files and encoding errors
- **Batch Processing**: Uploads JSONL files to OpenAI batch API with metadata tracking
- **Error Handling**: Exits on git command failures with descriptive error messages
- **Response Parsing**: Expects structured JSON with `reasoning`, `substantiveComments`, and `summary` fields

### Review Dashboard
- **Security**: Uses XSS-safe HTML escaping for all model outputs
- **API Integration**: Connects directly to OpenAI's API to fetch batch status and output files without requiring local state

## Dependencies

### GitHub Bot
- **Rust**: Async runtime with tokio
- **GitHub API**: JWT authentication and webhook processing
- **OpenAI API**: Batch processing integration

### Standalone Python Script
- **openai**: OpenAI API client
- **pyright**: Type checking
- **ruff**: Linting and formatting
- **uv**: Package management and virtual environments

## Nix Support

The repository includes a `flake.nix` for development environment setup with both Rust toolchain and Python 3, uv, and make.

## Security Guidelines

When working with this codebase, always follow these security practices:

### URL and Link Security
- **NEVER** directly assign user-controlled URLs to `href` attributes without validation
- **ALWAYS** sanitize URLs through appropriate validation functions (e.g., `getSafeRepoUrl()`)
- **ONLY** allow `http://` and `https://` protocols for links - reject `javascript:`, `data:`, `file:`, and other dangerous protocols
- When creating links from metadata that may include pull request URLs or other user-controlled data, validate ALL URL inputs, not just some

### XSS Prevention
- **ALWAYS** escape user-controlled content using `escapeHtml()` before inserting into DOM
- Be aware that browser href assignment does NOT sanitize protocols - explicit validation is required

## Development guidelines

- When fixing a bug, always write the test first that demonstrates the bug, commit it in failing state, and then make the fix to the code.
