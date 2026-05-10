use crate::WorkflowError;

/// Splits `/<name> [args...]` into the command name and its shell-split
/// args.
///
/// Returns `Ok(None)` for plain prose or for malformed-but-not-a-command
/// input (`/`, `/  foo`, no leading slash). Returns `Err` only when the
/// input looks like a command but the args fail to shell-parse, so the
/// caller can surface a precise error to the user.
pub fn parse_slash_command(text: &str) -> Result<Option<(&str, Vec<String>)>, WorkflowError> {
    let Some(body) = text.strip_prefix('/') else {
        return Ok(None);
    };
    let (name, rest) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], &body[idx..]),
        None => (body, ""),
    };
    if name.is_empty() {
        return Ok(None);
    }
    let args = shell_words::split(rest.trim())?;
    Ok(Some((name, args)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Option<(String, Vec<String>)> {
        parse_slash_command(text)
            .expect("parse should not error")
            .map(|(n, a)| (n.to_string(), a))
    }

    #[test]
    fn bare_name() {
        assert_eq!(parse("/plan"), Some(("plan".into(), vec![])));
    }

    #[test]
    fn name_and_args() {
        assert_eq!(
            parse("/plan foo bar"),
            Some(("plan".into(), vec!["foo".into(), "bar".into()])),
        );
    }

    #[test]
    fn quoted_arg_collapses() {
        assert_eq!(
            parse(r#"/plan "two words""#),
            Some(("plan".into(), vec!["two words".into()])),
        );
    }

    #[test]
    fn unterminated_quote_errors() {
        assert!(parse_slash_command("/plan 'unterminated").is_err());
    }

    #[test]
    fn slash_alone_is_not_a_command() {
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn slash_with_only_whitespace_name_is_not_a_command() {
        assert_eq!(parse("/ foo"), None);
    }

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse("hello there"), None);
    }

    #[test]
    fn leading_whitespace_does_not_strip() {
        // Match the user's literal input — a leading space means it isn't a
        // slash command. Don't silently re-interpret.
        assert_eq!(parse("  /plan"), None);
    }
}
