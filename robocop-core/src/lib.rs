pub mod git;
pub mod openai;
pub mod recording;
pub mod review;

pub use git::*;
pub use openai::*;
pub use recording::{
    CorrelationId, Direction, EventType, RecordedEvent, RecordingLogger, RecordingMiddleware,
    Sanitizer, ServiceType, CORRELATION_ID_HEADER,
};
pub use review::*;

/// Returns the library version as a short git hash.
///
/// Checks for ROBOCOP_GIT_HASH environment variable, which Nix builds provide.
/// Plain cargo builds fall back to "unknown".
pub fn get_library_version() -> String {
    if let Some(git_hash) = option_env!("ROBOCOP_GIT_HASH") {
        if git_hash.len() >= 8 {
            git_hash[..8].to_string()
        } else {
            git_hash.to_string()
        }
    } else {
        "unknown".to_string()
    }
}
