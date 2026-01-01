# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This repository contains four main components:

1. **Robocop Core Library** (`robocop-core/`): An async Rust library for OpenAI batch API integration and code review functionality
2. **Robocop Server** (`robocop-server/`): An async Rust-based GitHub webhook server for automated code reviews
3. **Robocop CLI** (`robocop-cli/`): A Rust CLI tool for automated code reviews (uses robocop-core library)
4. **Review Dashboard** (`dashboard.html`): A single HTML file to view the state of all code reviews performed using the tools in the repository

## Development Commands

This project uses Nix for reproducible development environments and Cargo workspaces for the Rust crates.

### Rust Workspace (robocop-core + robocop-server + robocop-cli)

The repository contains a Cargo workspace with three crates:
- `robocop-core`: Async library for OpenAI integration
- `robocop-server`: Async GitHub webhook server
- `robocop-cli`: CLI tool for code reviews

All commands should be run from the project root within the Nix environment:

- **Build All**: `nix develop --command cargo build`
- **Build Server Only**: `nix develop --command cargo build -p robocop-server`
- **Build CLI Only**: `nix develop --command cargo build -p robocop-cli`
- **Build Library Only**: `nix develop --command cargo build -p robocop-core`
- **Test All**: `nix develop --command cargo test`
- **Test Server Only**: `nix develop --command cargo test -p robocop-server`
- **Test CLI Only**: `nix develop --command cargo test -p robocop-cli`
- **Test Library Only**: `nix develop --command cargo test -p robocop-core`
- **Lint**: `nix develop --command cargo clippy --all-targets --all-features -- -D warnings`
- **Format**: `nix develop --command cargo fmt`
- **Format Check**: `nix develop --command cargo fmt --check`
- **Run Server**: `nix develop --command cargo run -p robocop-server`
- **Run CLI**: `nix develop --command cargo run -p robocop-cli -- [ARGS]`
- **Build Nix Package**: `nix build`

Before committing, always run Clippy and the formatter on the entire workspace.

## Architecture Overview

### Robocop Core Library

The `robocop-core` library provides async OpenAI batch API integration using tokio and reqwest.

**Key Components:**
- **OpenAI Client** (`src/openai.rs`): Async client using reqwest with reqwest-middleware for file uploads, batch creation, status checking, and cancellation
- **Review Types** (`src/review.rs`): ReviewMetadata, system prompt generation, and user prompt creation
- **Git Data Types** (`src/git.rs`): GitData struct for representing repository information

The library handles:
- Creating batch review requests
- Uploading JSONL files to OpenAI
- Managing batch metadata (commit hashes, branch names, PR URLs)
- Response format schemas for structured JSON output

### Robocop Server

This is a GitHub webhook-based bot that provides automated code reviews using OpenAI's batch API. The system uses async processing with background polling for batch completion and internally uses robocop-core types.

**Core Flow:**
1. **Webhook Reception**: Receives GitHub PR webhooks on `/webhook` endpoint
2. **Event Filtering**: Only processes events from the configured target user ID
3. **Diff Collection**: Gets diffs and changed file contents via GitHub API
4. **Batch Submission**: Submits review request to OpenAI batch API for async processing
5. **Polling Loop**: Background task polls batch status every 60 seconds
6. **Result Processing**: Updates PR comments with review results when batches complete

**Key Components:**
- **AppState**: Central state container with GitHub/OpenAI clients and pending batch tracking
- **Webhook Handler** (`robocop-server/src/webhook.rs`): Processes GitHub webhooks with signature verification
- **GitHub Client** (`robocop-server/src/github.rs`): Handles GitHub API interactions with JWT authentication
- **OpenAI Client** (`robocop-server/src/openai.rs`): Async wrapper around OpenAI batch API requests (uses types from robocop-core)
- **Recording System** (`robocop-server/src/recording/`): Optional HTTP request/response logging for debugging
- **Git Operations** (`robocop-server/src/git.rs`): Git ancestry checking for superseded commit handling

**Batch Processing:**
The system uses OpenAI's batch API for cost-effective async processing. When new commits supersede existing ones, it automatically cancels obsolete batches using git ancestry checks.

### Robocop CLI

The Rust CLI tool (`robocop-cli`) is a standalone command-line application that uses the robocop-core library to perform code reviews.

**Key Features:**
- **Subcommands**: `review` for code review, `list-models` to list available OpenAI models
- **Git Integration**: Automatically extracts diffs and file contents relative to merge-base
- **Dual Mode**: Supports both regular chat completions API and batch processing API
- **Model Selection**: Configure which OpenAI model to use via `--model` flag
- **Dry Run**: Preview prompts without making API calls
- **Flexible Options**: Customize reasoning effort, additional prompts, and included files
- **Error Handling**: Uses anyhow for comprehensive error handling with context

**Architecture:**
- Uses clap for CLI argument parsing with subcommands
- Leverages robocop-core for OpenAI integration and prompt management
- Async execution using tokio runtime with reqwest for HTTP calls
- Git operations implemented using std::process::Command

### Configuration

Environment variables required:
- `GITHUB_APP_ID`: GitHub App ID
- `GITHUB_PRIVATE_KEY`: GitHub App private key (PEM format, `\n` will be converted to newlines)
- `GITHUB_WEBHOOK_SECRET`: Secret for webhook signature verification
- `OPENAI_API_KEY`: OpenAI API key
- `TARGET_GITHUB_USER_ID`: Numeric GitHub user ID to monitor for pushes
- `PORT`: Server port (default: 3000)
- `STATE_DIR`: Directory for persistent state database (default: current directory)
- `RECORDING_ENABLED`: Enable HTTP recording (default: false)
- `RECORDING_LOG_PATH`: Recording log file path (default: recordings.jsonl)


### Review Dashboard

The `dashboard.html` file provides a web-based interface for monitoring batch review jobs:

- **Batch Monitoring**: View status, creation time, and metadata for all robocop batch jobs
- **Real-time Updates**: Auto-refresh functionality with configurable intervals (30s, 1m, 2m, 5m)
- **Review Results**: Display completed reviews with reasoning, summary, and substantive comments

**Usage**: Open `dashboard.html` in a web browser and enter your OpenAI API key to start monitoring batch reviews. When the Rust CLI is invoked using the `--batch` flag, or when the GitHub bot creates a review, only a batch ID is output - use the dashboard to monitor progress and retrieve results.

## Tool Usage

### Robocop CLI

The CLI uses subcommands. Available commands:
- `review`: Run a code review on the current git branch
- `list-models`: List available OpenAI models

#### Basic Review
```bash
nix develop --command cargo run -p robocop-cli -- review --api-key YOUR_API_KEY
```

#### Advanced Options
```bash
nix develop --command cargo run -p robocop-cli -- review \
  --api-key YOUR_API_KEY \
  --default-branch main \
  --reasoning-effort high \
  --model gpt-5-2025-08-07 \
  --additional-prompt "Focus on security issues" \
  --include-files config.py utils.py \
  --batch  # Use batch API for processing; it's cheaper (prints batch ID - use dashboard to view results)
```

#### Dry Run (Preview prompts)
```bash
nix develop --command cargo run -p robocop-cli -- review --dry-run
```

#### Using Environment Variable for API Key
```bash
export OPENAI_API_KEY=your_api_key_here
nix develop --command cargo run -p robocop-cli -- review
```

#### List Available Models
```bash
nix develop --command cargo run -p robocop-cli -- list-models
```

## Key Implementation Details

### Robocop Core Library
- **Async Design**: Uses tokio with reqwest and reqwest-middleware for all HTTP operations
- **Type Safety**: Strongly typed request/response structures with serde
- **Prompt Management**: System prompt loaded from prompt.txt, user prompt generation utilities
- **Metadata Tracking**: Captures git commit info, branch names, and PR URLs in batch metadata

### Robocop Server
- **Git Operations**: Git ancestry checking for superseded commit handling
- **Batch Processing**: Uses OpenAI's batch API for cost-effective async processing (wraps robocop-core types)
- **HTTP Recording**: Optional request/response logging for debugging
- **Async Runtime**: Uses tokio for async webhook handling and polling

### Robocop CLI
- **Async Runtime**: Uses tokio with single-threaded runtime for async operations
- **Git Operations**: Uses `std::process::Command` to execute git commands with `--no-ext-diff` flag for consistent diff output
- **File Reading**: Gracefully handles non-existent files and encoding errors using Option types
- **Dual Mode**: Supports both batch processing (via robocop-core) and direct chat completions API
- **Error Handling**: Uses anyhow for comprehensive error handling with context throughout the application
- **Response Parsing**: Expects structured JSON with `reasoning`, `substantiveComments`, and `summary` fields

### Review Dashboard
- **Security**: Uses XSS-safe HTML escaping for all model outputs
- **API Integration**: Connects directly to OpenAI's API to fetch batch status and output files without requiring local state

## Dependencies

### Robocop Core Library
- **tokio**: Async runtime
- **reqwest**: Async HTTP client for OpenAI API
- **reqwest-middleware**: HTTP middleware for request/response recording
- **serde/serde_json**: Serialization and deserialization
- **anyhow**: Error handling

### Robocop Server
- **robocop-core**: Core library for OpenAI integration
- **tokio**: Async runtime
- **axum**: Web framework for webhook handling
- **GitHub API**: JWT authentication and webhook processing
- **reqwest-middleware**: HTTP middleware for recording

### Robocop CLI
- **robocop-core**: Core library for OpenAI integration and prompt management
- **tokio**: Async runtime
- **clap**: Command-line argument parsing with derive macros
- **reqwest**: Async HTTP client for OpenAI chat completions API
- **serde/serde_json**: Serialization and deserialization
- **anyhow**: Error handling

## Nix Support

The repository includes a `flake.nix` for development environment setup with Rust toolchain.

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
- When running `git diff`, always do `git diff --no-ext-diff`.
