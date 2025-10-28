# TODO

## Known Issues

### OpenAI File Uploads Not Recorded

**Location**: `robocop-server/src/openai.rs:134`

The `upload_file` function bypasses the recording middleware because `reqwest_middleware` doesn't support multipart form uploads. This means file upload requests to OpenAI are not logged when HTTP recording is enabled.

**Impact**: Loss of observability for file upload operations during debugging.

**Possible Solutions**:
- Implement a custom recording solution that captures multipart uploads
- Add manual logging for file upload operations
- Investigate alternative HTTP client middleware that supports multipart forms

**Status**: Pre-existing issue (existed in `github-bot/src/openai.rs` before refactor)
