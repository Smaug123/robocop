/// Command parsing for @smaug123-robocop mentions in comments
use std::fmt;

/// Options for the review command
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewOptions {
    /// OpenAI model to use (e.g., "gpt-5-2025-08-07")
    pub model: Option<String>,
    /// Reasoning effort level (e.g., "high", "xhigh")
    pub reasoning_effort: Option<String>,
}

/// A parsed robocop command from a comment
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobocopCommand {
    /// Request a code review with optional parameters
    Review(ReviewOptions),
    /// Cancel all pending reviews for this PR
    Cancel,
    /// Enable automatic reviews
    EnableReviews,
    /// Disable automatic reviews
    DisableReviews,
}

/// A review state change that has been verified to come from an authorized user.
///
/// This type can only be constructed via [`try_authorize_state_change`], which requires
/// the comment's user ID to match the target user ID. This enforces at the type level
/// that state changes can only come from authorized users.
///
/// # Security
/// This type exists to prevent accidentally processing unauthorized commands.
/// By requiring this type in functions that change review state, we make it
/// a compile-time error to forget the authorization check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizedStateChange {
    /// Reviews should be enabled
    Enable,
    /// Reviews should be disabled
    Disable,
}

/// Try to extract an authorized state change from a comment.
///
/// Returns `Some(AuthorizedStateChange)` only if:
/// 1. The comment contains a valid enable-reviews or disable-reviews command
/// 2. The comment author's user ID matches the target user ID
///
/// # Arguments
/// * `comment_body` - The comment text to parse
/// * `comment_user_id` - The GitHub user ID of the comment author
/// * `target_user_id` - The authorized user's GitHub ID
///
/// # Security
/// This function is the only way to construct an `AuthorizedStateChange`,
/// ensuring that authorization is always checked before processing state changes.
pub fn try_authorize_state_change(
    comment_body: &str,
    comment_user_id: u64,
    target_user_id: u64,
) -> Option<AuthorizedStateChange> {
    // Authorization check: only the target user can change state
    if comment_user_id != target_user_id {
        return None;
    }

    // Parse the comment for a command
    if let ParseResult::Command(command) = parse_comment(comment_body) {
        match command {
            RobocopCommand::EnableReviews => Some(AuthorizedStateChange::Enable),
            RobocopCommand::DisableReviews => Some(AuthorizedStateChange::Disable),
            _ => None, // Other commands don't affect state
        }
    } else {
        None
    }
}

/// Result of parsing a comment for robocop commands
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    /// No mention of the bot in the comment
    NoMention,
    /// Bot was mentioned but the command was not recognized
    UnrecognizedCommand {
        /// The unrecognized command text that was attempted
        attempted: String,
    },
    /// A valid command was found
    Command(RobocopCommand),
}

impl fmt::Display for RobocopCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RobocopCommand::Review(opts) => {
                write!(f, "review")?;
                if let Some(model) = &opts.model {
                    write!(f, " model:{}", model)?;
                }
                if let Some(reasoning) = &opts.reasoning_effort {
                    write!(f, " reasoning:{}", reasoning)?;
                }
                Ok(())
            }
            RobocopCommand::Cancel => write!(f, "cancel"),
            RobocopCommand::EnableReviews => write!(f, "enable-reviews"),
            RobocopCommand::DisableReviews => write!(f, "disable-reviews"),
        }
    }
}

/// Normalize a model name by inserting a hyphen after "gpt" if missing.
///
/// OpenAI's naming convention uses "gpt-" prefix (e.g., "gpt-4", "gpt-5.2").
/// This autocorrects common mistakes like "gpt4" or "gpt5.2" to "gpt-4" or "gpt-5.2".
fn normalize_model_name(model: &str) -> String {
    // Check if it starts with "gpt" (case-insensitive) but not "gpt-"
    if model.len() > 3 {
        let prefix = &model[..3];
        let after_prefix = model.chars().nth(3).unwrap();
        if prefix.eq_ignore_ascii_case("gpt") && after_prefix != '-' {
            // Insert hyphen after "gpt"
            return format!("{}-{}", &model[..3], &model[3..]);
        }
    }
    model.to_string()
}

/// Parse key:value options from a space-separated string
///
/// Returns ReviewOptions with any recognized options filled in.
/// Unrecognized keys are ignored (for forward compatibility).
/// Empty values (e.g., "model:" or "model: " with space after colon) are ignored.
fn parse_review_options(options_str: &str) -> ReviewOptions {
    let mut opts = ReviewOptions::default();

    for token in options_str.split_whitespace() {
        if let Some((key, value)) = token.split_once(':') {
            // Skip empty values (e.g., "model:" without a value)
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            // Only lowercase the key for comparison, preserve value case
            match key.to_lowercase().as_str() {
                "model" => opts.model = Some(normalize_model_name(value)),
                "reasoning" => opts.reasoning_effort = Some(value.to_string()),
                _ => {} // Ignore unrecognized keys for forward compatibility
            }
        }
    }

    opts
}

/// Extract review options from text (e.g., PR description)
///
/// This function parses the text for a `@smaug123-robocop review` command and extracts
/// any options (model, reasoning) if present. It returns `None` if:
/// - No robocop mention is found
/// - The command is not `review`
/// - The command is unrecognized
///
/// This is useful for extracting default review options from a PR description
/// without treating it as a command that needs to be executed.
///
/// # Example
///
/// ```ignore
/// let body = "This PR adds feature X.\n\n@smaug123-robocop review model:gpt-5 reasoning:high";
/// let options = extract_review_options(body);
/// assert_eq!(options.unwrap().model, Some("gpt-5".to_string()));
/// ```
pub fn extract_review_options(text: &str) -> Option<ReviewOptions> {
    match parse_comment(text) {
        ParseResult::Command(RobocopCommand::Review(opts)) => Some(opts),
        _ => None,
    }
}

/// Parse a comment body for robocop commands
///
/// Returns a `ParseResult` indicating:
/// - `NoMention` if the bot was not mentioned
/// - `UnrecognizedCommand` if the bot was mentioned but the command was not recognized
/// - `Command` if a valid command was found
///
/// # Command Format
///
/// Commands must be on a single line. The format is:
/// `@smaug123-robocop <command> [options...]`
///
/// The mention must be at the beginning of a line (after trimming leading and trailing whitespace),
/// followed by whitespace and then the command name.
///
/// # First Mention Wins
///
/// The parser stops at the **first** line that starts with `@smaug123-robocop` (case-insensitive).
/// This applies even if the first mention is incomplete or has an unrecognized command:
///
/// - `@smaug123-robocop` alone (no command) returns `UnrecognizedCommand`
/// - `@smaug123-robocop typo` returns `UnrecognizedCommand { attempted: "typo" }`
///
/// In both cases, subsequent lines are **not scanned**, even if they contain valid commands.
/// This is intentional: it prevents confusion about which command will be executed when
/// a comment contains multiple mentions.
///
/// # Available Commands
///
/// - `review` - Request a code review
/// - `cancel` - Cancel pending reviews
/// - `enable-reviews` - Enable automatic reviews
/// - `disable-reviews` - Disable automatic reviews
///
/// # Review Options
///
/// The review command supports optional key:value parameters:
/// - `model:<model-name>` - OpenAI model to use
/// - `reasoning:<level>` - Reasoning effort level
///
/// Example: `@smaug123-robocop review model:gpt-5-2025-08-07 reasoning:xhigh`
pub fn parse_comment(body: &str) -> ParseResult {
    const MENTION: &str = "@smaug123-robocop";

    for line in body.lines() {
        let trimmed = line.trim();

        // Check if line starts with @smaug123-robocop (case-insensitive)
        // Use safe prefix extraction to avoid panicking on non-ASCII input
        let Some(prefix) = trimmed.get(..MENTION.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(MENTION) {
            continue;
        }

        // Safe to slice here because we already verified the boundary exists
        let rest = &trimmed[MENTION.len()..];

        // Require whitespace boundary after the mention, and a command must follow
        // Just "@smaug123-robocop" alone (rest is empty) is treated as unrecognized
        if rest.is_empty() {
            return ParseResult::UnrecognizedCommand {
                attempted: String::new(),
            };
        }

        // Check if the first character after the mention is whitespace
        if !rest.starts_with(|c: char| c.is_whitespace()) {
            // No whitespace boundary, e.g., "@smaug123-robocopreview"
            continue;
        }

        let command_part = rest.trim();

        // Parse the command - check for review with possible options
        // Split into command word and the rest using whitespace
        let (command_word, options_part) =
            match command_part.split_once(|c: char| c.is_whitespace()) {
                Some((cmd, rest)) => (cmd, rest.trim_start()),
                None => (command_part, ""),
            };

        if command_word.eq_ignore_ascii_case("review") {
            if options_part.is_empty() {
                return ParseResult::Command(RobocopCommand::Review(ReviewOptions::default()));
            } else {
                let opts = parse_review_options(options_part);
                return ParseResult::Command(RobocopCommand::Review(opts));
            }
        } else if command_word.eq_ignore_ascii_case("cancel") {
            return ParseResult::Command(RobocopCommand::Cancel);
        } else if command_word.eq_ignore_ascii_case("enable-reviews") {
            return ParseResult::Command(RobocopCommand::EnableReviews);
        } else if command_word.eq_ignore_ascii_case("disable-reviews") {
            return ParseResult::Command(RobocopCommand::DisableReviews);
        } else {
            // Bot was mentioned but command is unrecognized
            return ParseResult::UnrecognizedCommand {
                attempted: command_word.to_string(),
            };
        }
    }

    ParseResult::NoMention
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> ParseResult {
        ParseResult::Command(RobocopCommand::Review(ReviewOptions::default()))
    }

    fn review_with(model: Option<&str>, reasoning: Option<&str>) -> ParseResult {
        ParseResult::Command(RobocopCommand::Review(ReviewOptions {
            model: model.map(|s| s.to_string()),
            reasoning_effort: reasoning.map(|s| s.to_string()),
        }))
    }

    fn command(cmd: RobocopCommand) -> ParseResult {
        ParseResult::Command(cmd)
    }

    fn unrecognized(attempted: &str) -> ParseResult {
        ParseResult::UnrecognizedCommand {
            attempted: attempted.to_string(),
        }
    }

    #[test]
    fn test_parse_review_command() {
        assert_eq!(parse_comment("@smaug123-robocop review"), review());
        assert_eq!(parse_comment("@smaug123-robocop Review"), review());
        assert_eq!(parse_comment("  @smaug123-robocop review  "), review());
    }

    #[test]
    fn test_parse_review_with_model() {
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt-5-2025-08-07"),
            review_with(Some("gpt-5-2025-08-07"), None)
        );
    }

    #[test]
    fn test_parse_review_with_reasoning() {
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:xhigh"),
            review_with(None, Some("xhigh"))
        );
    }

    #[test]
    fn test_parse_review_with_both_options() {
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt-5-2025-08-07 reasoning:xhigh"),
            review_with(Some("gpt-5-2025-08-07"), Some("xhigh"))
        );
        // Order shouldn't matter
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:low model:gpt-4"),
            review_with(Some("gpt-4"), Some("low"))
        );
    }

    #[test]
    fn test_parse_review_options_case_insensitive_keys() {
        assert_eq!(
            parse_comment("@smaug123-robocop review MODEL:gpt-5 REASONING:high"),
            review_with(Some("gpt-5"), Some("high"))
        );
    }

    #[test]
    fn test_parse_review_ignores_unknown_options() {
        // Unknown keys should be silently ignored for forward compatibility
        assert_eq!(
            parse_comment("@smaug123-robocop review unknown:value model:gpt-5"),
            review_with(Some("gpt-5"), None)
        );
    }

    #[test]
    fn test_parse_cancel_command() {
        assert_eq!(
            parse_comment("@smaug123-robocop cancel"),
            command(RobocopCommand::Cancel)
        );
        assert_eq!(
            parse_comment("@smaug123-robocop Cancel"),
            command(RobocopCommand::Cancel)
        );
    }

    #[test]
    fn test_parse_multiline_comment() {
        let comment = "Hey there,\n\n@smaug123-robocop review\n\nThanks!";
        assert_eq!(parse_comment(comment), review());
    }

    #[test]
    fn test_parse_multiline_comment_with_options() {
        let comment =
            "Hey there,\n\n@smaug123-robocop review model:gpt-5 reasoning:high\n\nThanks!";
        assert_eq!(
            parse_comment(comment),
            review_with(Some("gpt-5"), Some("high"))
        );
    }

    #[test]
    fn test_no_mention() {
        assert_eq!(
            parse_comment("This is just a regular comment"),
            ParseResult::NoMention
        );
        assert_eq!(
            parse_comment("smaug123-robocop review"),
            ParseResult::NoMention
        ); // Missing @
    }

    #[test]
    fn test_unrecognized_command() {
        // Bot mentioned but command not recognized
        assert_eq!(parse_comment("@smaug123-robocop"), unrecognized("")); // No command
        assert_eq!(
            parse_comment("@smaug123-robocop unknown"),
            unrecognized("unknown")
        ); // Invalid command
        assert_eq!(
            parse_comment("@smaug123-robocop help"),
            unrecognized("help")
        );
        assert_eq!(
            parse_comment("@smaug123-robocop reveiw"),
            unrecognized("reveiw")
        ); // Typo
    }

    #[test]
    fn test_first_command_wins() {
        let comment = "@smaug123-robocop review\n@smaug123-robocop cancel";
        assert_eq!(parse_comment(comment), review());
    }

    #[test]
    fn test_first_mention_wins_even_if_unrecognized() {
        // If the first mention has an unrecognized command, subsequent valid commands are NOT scanned.
        // This is intentional: it prevents confusion about which command will be executed.
        let comment = "@smaug123-robocop typo\n@smaug123-robocop review";
        assert_eq!(
            parse_comment(comment),
            unrecognized("typo"),
            "First mention with unrecognized command should stop scanning; valid command on second line is ignored"
        );
    }

    #[test]
    fn test_first_mention_wins_even_if_empty() {
        // If the first mention has no command at all, subsequent valid commands are NOT scanned.
        let comment = "@smaug123-robocop\n@smaug123-robocop review";
        assert_eq!(
            parse_comment(comment),
            unrecognized(""),
            "First mention with no command should stop scanning; valid command on second line is ignored"
        );
    }

    #[test]
    fn test_commands_must_be_on_single_line() {
        // The command must be on the same line as the mention - you cannot split across lines
        let comment = "@smaug123-robocop\nreview";
        assert_eq!(
            parse_comment(comment),
            unrecognized(""),
            "Command must be on same line as mention; 'review' on next line should not be recognized"
        );

        // Even with extra whitespace, the command must follow the mention on the same line
        let comment_with_space = "@smaug123-robocop   \n   review";
        assert_eq!(
            parse_comment(comment_with_space),
            unrecognized(""),
            "Command cannot be on a separate line even with whitespace"
        );
    }

    #[test]
    fn test_midline_mention_ignored() {
        // @smaug123-robocop must be at the start of a line
        assert_eq!(
            parse_comment("I think @smaug123-robocop review would be great"),
            ParseResult::NoMention
        );
    }

    #[test]
    fn test_case_insensitive_mention() {
        // The mention should be case-insensitive
        assert_eq!(parse_comment("@Smaug123-Robocop review"), review());
        assert_eq!(
            parse_comment("@SMAUG123-ROBOCOP cancel"),
            command(RobocopCommand::Cancel)
        );
        assert_eq!(parse_comment("@SmAuG123-RoBoCop review"), review());
    }

    #[test]
    fn test_requires_whitespace_boundary() {
        // Mention must be followed by whitespace before the command
        assert_eq!(
            parse_comment("@smaug123-robocopreview"),
            ParseResult::NoMention,
            "Should reject mention without whitespace separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocopcancel"),
            ParseResult::NoMention,
            "Should reject mention without whitespace separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop-review"),
            ParseResult::NoMention,
            "Should treat hyphenated suffix as different username, not a bot mention"
        );

        // Valid commands with whitespace
        assert_eq!(
            parse_comment("@smaug123-robocop review"),
            review(),
            "Should accept command with space separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop\treview"),
            review(),
            "Should accept command with tab separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop  review"),
            review(),
            "Should accept command with multiple spaces"
        );
    }

    #[test]
    fn test_parse_enable_reviews_command() {
        assert_eq!(
            parse_comment("@smaug123-robocop enable-reviews"),
            command(RobocopCommand::EnableReviews)
        );
        assert_eq!(
            parse_comment("@smaug123-robocop Enable-Reviews"),
            command(RobocopCommand::EnableReviews)
        );
        assert_eq!(
            parse_comment("  @smaug123-robocop ENABLE-REVIEWS  "),
            command(RobocopCommand::EnableReviews)
        );
    }

    #[test]
    fn test_parse_disable_reviews_command() {
        assert_eq!(
            parse_comment("@smaug123-robocop disable-reviews"),
            command(RobocopCommand::DisableReviews)
        );
        assert_eq!(
            parse_comment("@smaug123-robocop Disable-Reviews"),
            command(RobocopCommand::DisableReviews)
        );
    }

    #[test]
    fn test_enable_disable_multiline() {
        let comment = "Please review this.\n\n@smaug123-robocop enable-reviews";
        assert_eq!(
            parse_comment(comment),
            command(RobocopCommand::EnableReviews)
        );
    }

    #[test]
    fn test_display_review_with_options() {
        let cmd = RobocopCommand::Review(ReviewOptions {
            model: Some("gpt-5".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
        });
        assert_eq!(cmd.to_string(), "review model:gpt-5 reasoning:xhigh");

        let cmd = RobocopCommand::Review(ReviewOptions {
            model: Some("gpt-5".to_string()),
            reasoning_effort: None,
        });
        assert_eq!(cmd.to_string(), "review model:gpt-5");

        let cmd = RobocopCommand::Review(ReviewOptions {
            model: None,
            reasoning_effort: Some("high".to_string()),
        });
        assert_eq!(cmd.to_string(), "review reasoning:high");

        let cmd = RobocopCommand::Review(ReviewOptions::default());
        assert_eq!(cmd.to_string(), "review");
    }

    #[test]
    fn test_review_multiple_whitespace_before_options() {
        // Multiple spaces between "review" and options should work
        assert_eq!(
            parse_comment("@smaug123-robocop review  model:gpt-5"),
            review_with(Some("gpt-5"), None),
            "Should accept multiple spaces between review and options"
        );
        // Tab between "review" and options should work
        assert_eq!(
            parse_comment("@smaug123-robocop review\tmodel:gpt-5"),
            review_with(Some("gpt-5"), None),
            "Should accept tab between review and options"
        );
    }

    #[test]
    fn test_option_values_preserve_case() {
        // Model names can be case-sensitive
        assert_eq!(
            parse_comment("@smaug123-robocop review model:GPT-5-Turbo"),
            review_with(Some("GPT-5-Turbo"), None),
            "Option values should preserve their original case"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:XHIGH"),
            review_with(None, Some("XHIGH")),
            "Reasoning values should preserve their original case"
        );
    }

    #[test]
    fn test_model_name_autocorrects_missing_hyphen() {
        // OpenAI models use "gpt-" prefix with hyphen; autocorrect if user omits it
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt5.2"),
            review_with(Some("gpt-5.2"), None),
            "gpt5.2 should be autocorrected to gpt-5.2"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop review model:GPT4"),
            review_with(Some("GPT-4"), None),
            "GPT4 should be autocorrected to GPT-4"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt4o"),
            review_with(Some("gpt-4o"), None),
            "gpt4o should be autocorrected to gpt-4o"
        );
        // Already correct - should not be modified
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt-5.2"),
            review_with(Some("gpt-5.2"), None),
            "gpt-5.2 should remain unchanged"
        );
        // Non-gpt models should not be modified
        assert_eq!(
            parse_comment("@smaug123-robocop review model:o1"),
            review_with(Some("o1"), None),
            "o1 should remain unchanged"
        );
    }

    #[test]
    fn test_non_ascii_line_does_not_panic() {
        // Lines starting with non-ASCII characters should not cause a panic
        // when we check if they start with the mention
        // This tests for UTF-8 safety in the prefix check
        assert_eq!(
            parse_comment("🔥🔥🔥🔥🔥 review"),
            ParseResult::NoMention,
            "Non-ASCII line should return NoMention, not panic"
        );
        assert_eq!(
            parse_comment("日本語テスト"),
            ParseResult::NoMention,
            "Non-ASCII line should return NoMention, not panic"
        );
        // Multi-byte characters with length > MENTION.len() in bytes but fewer chars
        assert_eq!(
            parse_comment("🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥"),
            ParseResult::NoMention,
            "Line of emojis should return NoMention, not panic"
        );
        // Mix of ASCII and non-ASCII that could trip up byte slicing
        assert_eq!(
            parse_comment("@日本語 review"),
            ParseResult::NoMention,
            "Line with non-ASCII after @ should not panic"
        );
    }

    #[test]
    fn test_option_with_space_after_colon() {
        // "model: gpt-5" (space after colon) should either:
        // - Parse "gpt-5" as the value (accepting space after colon), OR
        // - Return None for model (ignoring empty value)
        // It should NOT return Some("") for model
        let result = parse_comment("@smaug123-robocop review model: gpt-5");
        match &result {
            ParseResult::Command(RobocopCommand::Review(opts)) => {
                // Either model is None (empty value ignored) or model is Some("gpt-5")
                assert!(
                    opts.model.is_none() || opts.model.as_deref() == Some("gpt-5"),
                    "model should be None or 'gpt-5', not empty string. Got: {:?}",
                    opts.model
                );
            }
            _ => panic!("Expected Command(Review(...)), got {:?}", result),
        }
    }

    // Tests for AuthorizedStateChange type-safe authorization

    #[test]
    fn test_authorized_state_change_enable_from_target_user() {
        let target_user_id = 12345u64;
        let result = try_authorize_state_change(
            "@smaug123-robocop enable-reviews",
            target_user_id,
            target_user_id,
        );
        assert_eq!(result, Some(AuthorizedStateChange::Enable));
    }

    #[test]
    fn test_authorized_state_change_disable_from_target_user() {
        let target_user_id = 12345u64;
        let result = try_authorize_state_change(
            "@smaug123-robocop disable-reviews",
            target_user_id,
            target_user_id,
        );
        assert_eq!(result, Some(AuthorizedStateChange::Disable));
    }

    #[test]
    fn test_authorized_state_change_rejected_for_unauthorized_user() {
        let target_user_id = 12345u64;
        let unauthorized_user_id = 99999u64;

        // Enable command from unauthorized user should be rejected
        let result = try_authorize_state_change(
            "@smaug123-robocop enable-reviews",
            unauthorized_user_id,
            target_user_id,
        );
        assert_eq!(
            result, None,
            "Unauthorized user's enable-reviews should be rejected"
        );

        // Disable command from unauthorized user should be rejected
        let result = try_authorize_state_change(
            "@smaug123-robocop disable-reviews",
            unauthorized_user_id,
            target_user_id,
        );
        assert_eq!(
            result, None,
            "Unauthorized user's disable-reviews should be rejected"
        );
    }

    #[test]
    fn test_authorized_state_change_non_state_commands_return_none() {
        let target_user_id = 12345u64;

        // Review command doesn't affect state
        let result =
            try_authorize_state_change("@smaug123-robocop review", target_user_id, target_user_id);
        assert_eq!(
            result, None,
            "review command should not return state change"
        );

        // Cancel command doesn't affect state
        let result =
            try_authorize_state_change("@smaug123-robocop cancel", target_user_id, target_user_id);
        assert_eq!(
            result, None,
            "cancel command should not return state change"
        );
    }

    #[test]
    fn test_authorized_state_change_no_mention_returns_none() {
        let target_user_id = 12345u64;
        let result =
            try_authorize_state_change("Just a regular comment", target_user_id, target_user_id);
        assert_eq!(result, None);
    }

    // Tests for extract_review_options

    #[test]
    fn test_extract_review_options_with_model_and_reasoning() {
        let body =
            "This PR adds a new feature.\n\n@smaug123-robocop review model:gpt-5.2 reasoning:xhigh";
        let opts = extract_review_options(body);
        assert!(opts.is_some());
        let opts = opts.unwrap();
        assert_eq!(opts.model, Some("gpt-5.2".to_string()));
        assert_eq!(opts.reasoning_effort, Some("xhigh".to_string()));
    }

    #[test]
    fn test_extract_review_options_with_model_only() {
        let body = "@smaug123-robocop review model:gpt-4-turbo";
        let opts = extract_review_options(body);
        assert!(opts.is_some());
        let opts = opts.unwrap();
        assert_eq!(opts.model, Some("gpt-4-turbo".to_string()));
        assert_eq!(opts.reasoning_effort, None);
    }

    #[test]
    fn test_extract_review_options_with_reasoning_only() {
        let body = "@smaug123-robocop review reasoning:low";
        let opts = extract_review_options(body);
        assert!(opts.is_some());
        let opts = opts.unwrap();
        assert_eq!(opts.model, None);
        assert_eq!(opts.reasoning_effort, Some("low".to_string()));
    }

    #[test]
    fn test_extract_review_options_plain_review() {
        let body = "@smaug123-robocop review";
        let opts = extract_review_options(body);
        assert!(opts.is_some());
        let opts = opts.unwrap();
        assert_eq!(opts.model, None);
        assert_eq!(opts.reasoning_effort, None);
    }

    #[test]
    fn test_extract_review_options_no_mention() {
        let body = "Just a regular PR description without any bot mention.";
        let opts = extract_review_options(body);
        assert!(opts.is_none());
    }

    #[test]
    fn test_extract_review_options_disable_reviews() {
        // disable-reviews should return None, not options
        let body = "@smaug123-robocop disable-reviews";
        let opts = extract_review_options(body);
        assert!(opts.is_none());
    }

    #[test]
    fn test_extract_review_options_unrecognized_command() {
        let body = "@smaug123-robocop typo";
        let opts = extract_review_options(body);
        assert!(opts.is_none());
    }

    #[test]
    fn test_extract_review_options_in_multiline_pr_body() {
        let body = r#"## Summary

This PR implements feature X.

## Test Plan

- Run unit tests
- Manual testing

@smaug123-robocop review model:o1 reasoning:high

## Notes

Some additional notes here."#;
        let opts = extract_review_options(body);
        assert!(opts.is_some());
        let opts = opts.unwrap();
        assert_eq!(opts.model, Some("o1".to_string()));
        assert_eq!(opts.reasoning_effort, Some("high".to_string()));
    }
}
