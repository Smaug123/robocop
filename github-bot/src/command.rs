/// Command parsing for @robocop mentions in comments
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
/// Commands must start with @robocop at the beginning of a line.
pub fn parse_comment(body: &str) -> Option<RobocopCommand> {
    for line in body.lines() {
        let trimmed = line.trim();

        // Check if line starts with @robocop
        if let Some(rest) = trimmed.strip_prefix("@robocop") {
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
            parse_comment("@robocop review"),
            Some(RobocopCommand::Review)
        );
        assert_eq!(
            parse_comment("@robocop Review"),
            Some(RobocopCommand::Review)
        );
        assert_eq!(
            parse_comment("  @robocop review  "),
            Some(RobocopCommand::Review)
        );
    }

    #[test]
    fn test_parse_cancel_command() {
        assert_eq!(
            parse_comment("@robocop cancel"),
            Some(RobocopCommand::Cancel)
        );
        assert_eq!(
            parse_comment("@robocop Cancel"),
            Some(RobocopCommand::Cancel)
        );
    }

    #[test]
    fn test_parse_multiline_comment() {
        let comment = "Hey there,\n\n@robocop review\n\nThanks!";
        assert_eq!(parse_comment(comment), Some(RobocopCommand::Review));
    }

    #[test]
    fn test_no_command() {
        assert_eq!(parse_comment("This is just a regular comment"), None);
        assert_eq!(parse_comment("robocop review"), None); // Missing @
        assert_eq!(parse_comment("@robocop"), None); // No command
        assert_eq!(parse_comment("@robocop unknown"), None); // Invalid command
    }

    #[test]
    fn test_first_command_wins() {
        let comment = "@robocop review\n@robocop cancel";
        assert_eq!(parse_comment(comment), Some(RobocopCommand::Review));
    }

    #[test]
    fn test_midline_mention_ignored() {
        // @robocop must be at the start of a line
        assert_eq!(
            parse_comment("I think @robocop review would be great"),
            None
        );
    }
}
