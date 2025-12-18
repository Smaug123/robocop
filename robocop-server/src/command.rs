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
                "model" => opts.model = Some(value.to_string()),
                "reasoning" => opts.reasoning_effort = Some(value.to_string()),
                _ => {} // Ignore unrecognized keys for forward compatibility
            }
        }
    }

    opts
}

/// Parse a comment body for robocop commands
///
/// Returns the first valid command found in the comment, or None if no command is found.
/// Commands must start with @smaug123-robocop at the beginning of a line (after trimming leading and trailing whitespace),
/// followed by whitespace and then the command name.
///
/// The review command supports optional key:value parameters:
/// - `model:<model-name>` - OpenAI model to use
/// - `reasoning:<level>` - Reasoning effort level
///
/// Example: `@smaug123-robocop review model:gpt-5-2025-08-07 reasoning:xhigh`
pub fn parse_comment(body: &str) -> Option<RobocopCommand> {
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

        // Require whitespace boundary after the mention
        // Accept only if rest is empty (just the mention) or starts with whitespace
        if rest.is_empty() {
            // Just "@smaug123-robocop" with no command
            continue;
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
                return Some(RobocopCommand::Review(ReviewOptions::default()));
            } else {
                let opts = parse_review_options(options_part);
                return Some(RobocopCommand::Review(opts));
            }
        } else if command_word.eq_ignore_ascii_case("cancel") {
            return Some(RobocopCommand::Cancel);
        } else if command_word.eq_ignore_ascii_case("enable-reviews") {
            return Some(RobocopCommand::EnableReviews);
        } else if command_word.eq_ignore_ascii_case("disable-reviews") {
            return Some(RobocopCommand::DisableReviews);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> RobocopCommand {
        RobocopCommand::Review(ReviewOptions::default())
    }

    fn review_with(model: Option<&str>, reasoning: Option<&str>) -> RobocopCommand {
        RobocopCommand::Review(ReviewOptions {
            model: model.map(|s| s.to_string()),
            reasoning_effort: reasoning.map(|s| s.to_string()),
        })
    }

    #[test]
    fn test_parse_review_command() {
        assert_eq!(parse_comment("@smaug123-robocop review"), Some(review()));
        assert_eq!(parse_comment("@smaug123-robocop Review"), Some(review()));
        assert_eq!(
            parse_comment("  @smaug123-robocop review  "),
            Some(review())
        );
    }

    #[test]
    fn test_parse_review_with_model() {
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt-5-2025-08-07"),
            Some(review_with(Some("gpt-5-2025-08-07"), None))
        );
    }

    #[test]
    fn test_parse_review_with_reasoning() {
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:xhigh"),
            Some(review_with(None, Some("xhigh")))
        );
    }

    #[test]
    fn test_parse_review_with_both_options() {
        assert_eq!(
            parse_comment("@smaug123-robocop review model:gpt-5-2025-08-07 reasoning:xhigh"),
            Some(review_with(Some("gpt-5-2025-08-07"), Some("xhigh")))
        );
        // Order shouldn't matter
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:low model:gpt-4"),
            Some(review_with(Some("gpt-4"), Some("low")))
        );
    }

    #[test]
    fn test_parse_review_options_case_insensitive_keys() {
        assert_eq!(
            parse_comment("@smaug123-robocop review MODEL:gpt-5 REASONING:high"),
            Some(review_with(Some("gpt-5"), Some("high")))
        );
    }

    #[test]
    fn test_parse_review_ignores_unknown_options() {
        // Unknown keys should be silently ignored for forward compatibility
        assert_eq!(
            parse_comment("@smaug123-robocop review unknown:value model:gpt-5"),
            Some(review_with(Some("gpt-5"), None))
        );
    }

    #[test]
    fn test_parse_cancel_command() {
        assert_eq!(
            parse_comment("@smaug123-robocop cancel"),
            Some(RobocopCommand::Cancel)
        );
        assert_eq!(
            parse_comment("@smaug123-robocop Cancel"),
            Some(RobocopCommand::Cancel)
        );
    }

    #[test]
    fn test_parse_multiline_comment() {
        let comment = "Hey there,\n\n@smaug123-robocop review\n\nThanks!";
        assert_eq!(parse_comment(comment), Some(review()));
    }

    #[test]
    fn test_parse_multiline_comment_with_options() {
        let comment =
            "Hey there,\n\n@smaug123-robocop review model:gpt-5 reasoning:high\n\nThanks!";
        assert_eq!(
            parse_comment(comment),
            Some(review_with(Some("gpt-5"), Some("high")))
        );
    }

    #[test]
    fn test_no_command() {
        assert_eq!(parse_comment("This is just a regular comment"), None);
        assert_eq!(parse_comment("smaug123-robocop review"), None); // Missing @
        assert_eq!(parse_comment("@smaug123-robocop"), None); // No command
        assert_eq!(parse_comment("@smaug123-robocop unknown"), None); // Invalid command
    }

    #[test]
    fn test_first_command_wins() {
        let comment = "@smaug123-robocop review\n@smaug123-robocop cancel";
        assert_eq!(parse_comment(comment), Some(review()));
    }

    #[test]
    fn test_midline_mention_ignored() {
        // @smaug123-robocop must be at the start of a line
        assert_eq!(
            parse_comment("I think @smaug123-robocop review would be great"),
            None
        );
    }

    #[test]
    fn test_case_insensitive_mention() {
        // The mention should be case-insensitive
        assert_eq!(parse_comment("@Smaug123-Robocop review"), Some(review()));
        assert_eq!(
            parse_comment("@SMAUG123-ROBOCOP cancel"),
            Some(RobocopCommand::Cancel)
        );
        assert_eq!(parse_comment("@SmAuG123-RoBoCop review"), Some(review()));
    }

    #[test]
    fn test_requires_whitespace_boundary() {
        // Mention must be followed by whitespace before the command
        assert_eq!(
            parse_comment("@smaug123-robocopreview"),
            None,
            "Should reject mention without whitespace separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocopcancel"),
            None,
            "Should reject mention without whitespace separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop-review"),
            None,
            "Should reject mention with hyphen separator instead of whitespace"
        );

        // Valid commands with whitespace
        assert_eq!(
            parse_comment("@smaug123-robocop review"),
            Some(review()),
            "Should accept command with space separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop\treview"),
            Some(review()),
            "Should accept command with tab separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop  review"),
            Some(review()),
            "Should accept command with multiple spaces"
        );
    }

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

    #[test]
    fn test_display_review_with_options() {
        let cmd = review_with(Some("gpt-5"), Some("xhigh"));
        assert_eq!(cmd.to_string(), "review model:gpt-5 reasoning:xhigh");

        let cmd = review_with(Some("gpt-5"), None);
        assert_eq!(cmd.to_string(), "review model:gpt-5");

        let cmd = review_with(None, Some("high"));
        assert_eq!(cmd.to_string(), "review reasoning:high");

        let cmd = review();
        assert_eq!(cmd.to_string(), "review");
    }

    #[test]
    fn test_review_multiple_whitespace_before_options() {
        // Multiple spaces between "review" and options should work
        assert_eq!(
            parse_comment("@smaug123-robocop review  model:gpt-5"),
            Some(review_with(Some("gpt-5"), None)),
            "Should accept multiple spaces between review and options"
        );
        // Tab between "review" and options should work
        assert_eq!(
            parse_comment("@smaug123-robocop review\tmodel:gpt-5"),
            Some(review_with(Some("gpt-5"), None)),
            "Should accept tab between review and options"
        );
    }

    #[test]
    fn test_option_values_preserve_case() {
        // Model names can be case-sensitive
        assert_eq!(
            parse_comment("@smaug123-robocop review model:GPT-5-Turbo"),
            Some(review_with(Some("GPT-5-Turbo"), None)),
            "Option values should preserve their original case"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop review reasoning:XHIGH"),
            Some(review_with(None, Some("XHIGH"))),
            "Reasoning values should preserve their original case"
        );
    }

    #[test]
    fn test_non_ascii_line_does_not_panic() {
        // Lines starting with non-ASCII characters should not cause a panic
        // when we check if they start with the mention
        // This tests for UTF-8 safety in the prefix check
        assert_eq!(
            parse_comment("🔥🔥🔥🔥🔥 review"),
            None,
            "Non-ASCII line should return None, not panic"
        );
        assert_eq!(
            parse_comment("日本語テスト"),
            None,
            "Non-ASCII line should return None, not panic"
        );
        // Multi-byte characters with length > MENTION.len() in bytes but fewer chars
        assert_eq!(
            parse_comment("🔥🔥🔥🔥🔥🔥🔥🔥🔥🔥"),
            None,
            "Line of emojis should return None, not panic"
        );
        // Mix of ASCII and non-ASCII that could trip up byte slicing
        assert_eq!(
            parse_comment("@日本語 review"),
            None,
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
            Some(RobocopCommand::Review(opts)) => {
                // Either model is None (empty value ignored) or model is Some("gpt-5")
                assert!(
                    opts.model.is_none() || opts.model.as_deref() == Some("gpt-5"),
                    "model should be None or 'gpt-5', not empty string. Got: {:?}",
                    opts.model
                );
            }
            _ => panic!("Expected Some(Review(...)), got {:?}", result),
        }
    }
}
