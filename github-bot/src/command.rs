/// Command parsing for @smaug123-robocop mentions in comments
use std::fmt;

/// A parsed robocop command from a comment
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobocopCommand {
    /// Request a code review
    Review,
    /// Cancel all pending reviews for this PR
    Cancel,
}

impl fmt::Display for RobocopCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RobocopCommand::Review => write!(f, "review"),
            RobocopCommand::Cancel => write!(f, "cancel"),
        }
    }
}

/// Parse a comment body for robocop commands
///
/// Returns the first valid command found in the comment, or None if no command is found.
/// Commands must start with @smaug123-robocop at the beginning of a line (after trimming leading and trailing whitespace),
/// followed by whitespace and then the command name.
pub fn parse_comment(body: &str) -> Option<RobocopCommand> {
    for line in body.lines() {
        let trimmed = line.trim();

        // Check if line starts with @smaug123-robocop (case-insensitive)
        let lowercase = trimmed.to_lowercase();
        if let Some(rest) = lowercase.strip_prefix("@smaug123-robocop") {
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

            // Parse the command
            if command_part.eq_ignore_ascii_case("review") {
                return Some(RobocopCommand::Review);
            } else if command_part.eq_ignore_ascii_case("cancel") {
                return Some(RobocopCommand::Cancel);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_review_command() {
        assert_eq!(
            parse_comment("@smaug123-robocop review"),
            Some(RobocopCommand::Review)
        );
        assert_eq!(
            parse_comment("@smaug123-robocop Review"),
            Some(RobocopCommand::Review)
        );
        assert_eq!(
            parse_comment("  @smaug123-robocop review  "),
            Some(RobocopCommand::Review)
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
        assert_eq!(parse_comment(comment), Some(RobocopCommand::Review));
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
        assert_eq!(parse_comment(comment), Some(RobocopCommand::Review));
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
        assert_eq!(
            parse_comment("@Smaug123-Robocop review"),
            Some(RobocopCommand::Review)
        );
        assert_eq!(
            parse_comment("@SMAUG123-ROBOCOP cancel"),
            Some(RobocopCommand::Cancel)
        );
        assert_eq!(
            parse_comment("@SmAuG123-RoBoCop review"),
            Some(RobocopCommand::Review)
        );
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
            Some(RobocopCommand::Review),
            "Should accept command with space separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop\treview"),
            Some(RobocopCommand::Review),
            "Should accept command with tab separator"
        );
        assert_eq!(
            parse_comment("@smaug123-robocop  review"),
            Some(RobocopCommand::Review),
            "Should accept command with multiple spaces"
        );
    }
}
