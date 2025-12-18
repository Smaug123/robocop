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

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// Returns the library version as a short git hash.
///
/// First checks for ROBOCOP_GIT_HASH environment variable (set by Nix builds),
/// then falls back to the git hash detected at build time by the `built` crate.
pub fn get_library_version() -> String {
    // First check for git hash from Nix build environment
    if let Some(git_hash) = option_env!("ROBOCOP_GIT_HASH") {
        if git_hash.len() >= 8 {
            git_hash[..8].to_string()
        } else {
            git_hash.to_string()
        }
    } else if let Some(git_hash) = built_info::GIT_COMMIT_HASH {
        // Fall back to built crate's git detection (for cargo builds)
        if git_hash.len() >= 8 {
            git_hash[..8].to_string()
        } else {
            git_hash.to_string()
        }
    } else {
        "unknown".to_string()
    }
}
