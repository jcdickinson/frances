//! PoC-grade CommonMark inline scanner: `**bold**` / `__bold__` and
//! `*italic*` / `_italic_`. No nesting, no escaping, no other markup.
//! Unclosed delimiters at end-of-input render literal.
//!
//! Greedy single-pass; bold matches are attempted before italic so the
//! literal `**` opener doesn't get consumed by the italic scanner.

use ratatui::style::{Modifier, Style};

#[derive(Debug, Clone, PartialEq)]
pub struct StyledSpan {
    pub text: String,
    pub style: Style,
}

impl StyledSpan {
    pub fn plain<S: Into<String>>(text: S) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn bold<S: Into<String>>(text: S) -> Self {
        Self {
            text: text.into(),
            style: Style::default().add_modifier(Modifier::BOLD),
        }
    }

    pub fn italic<S: Into<String>>(text: S) -> Self {
        Self {
            text: text.into(),
            style: Style::default().add_modifier(Modifier::ITALIC),
        }
    }
}

/// Parse one paragraph into styled spans. The scanner walks Unicode
/// scalars (chars), not bytes, so multibyte characters can sit either
/// side of a `*` / `_` delimiter without splitting.
pub fn parse_inline(text: &str) -> Vec<StyledSpan> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<StyledSpan> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    let len = chars.len();

    while i < len {
        let c = chars[i];
        if c == '*' || c == '_' {
            if i + 1 < len
                && chars[i + 1] == c
                && let Some(close) = find_double(&chars, i + 2, c)
            {
                flush_plain(&mut out, &mut plain);
                let inner: String = chars[i + 2..close].iter().collect();
                out.push(StyledSpan::bold(inner));
                i = close + 2;
                continue;
            }
            if let Some(close) = find_single(&chars, i + 1, c) {
                flush_plain(&mut out, &mut plain);
                let inner: String = chars[i + 1..close].iter().collect();
                out.push(StyledSpan::italic(inner));
                i = close + 1;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush_plain(&mut out, &mut plain);
    out
}

fn flush_plain(out: &mut Vec<StyledSpan>, plain: &mut String) {
    if !plain.is_empty() {
        out.push(StyledSpan::plain(std::mem::take(plain)));
    }
}

fn find_double(chars: &[char], start: usize, c: char) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == c && chars[i + 1] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_single(chars: &[char], start: usize, c: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        assert_eq!(
            parse_inline("hello world"),
            vec![StyledSpan::plain("hello world")]
        );
    }

    #[test]
    fn bold_double_asterisk() {
        let out = parse_inline("**bold**");
        assert_eq!(out, vec![StyledSpan::bold("bold")]);
    }

    #[test]
    fn bold_double_underscore() {
        let out = parse_inline("__bold__");
        assert_eq!(out, vec![StyledSpan::bold("bold")]);
    }

    #[test]
    fn italic_single_asterisk() {
        let out = parse_inline("*italic*");
        assert_eq!(out, vec![StyledSpan::italic("italic")]);
    }

    #[test]
    fn italic_single_underscore() {
        let out = parse_inline("_italic_");
        assert_eq!(out, vec![StyledSpan::italic("italic")]);
    }

    #[test]
    fn bold_inside_plain() {
        let out = parse_inline("a **B** c");
        assert_eq!(
            out,
            vec![
                StyledSpan::plain("a "),
                StyledSpan::bold("B"),
                StyledSpan::plain(" c"),
            ],
        );
    }

    #[test]
    fn unclosed_delimiter_is_literal() {
        let out = parse_inline("*unclosed");
        assert_eq!(out, vec![StyledSpan::plain("*unclosed")]);
    }

    #[test]
    fn bold_preferred_over_italic_for_leading_double() {
        // `**foo**` would parse as italic("") + plain("foo") + italic("")
        // under naive single-delimiter precedence. Bold-first avoids it.
        let out = parse_inline("**foo**");
        assert_eq!(out, vec![StyledSpan::bold("foo")]);
    }

    #[test]
    fn multibyte_inside_bold() {
        let out = parse_inline("**héllo**");
        assert_eq!(out, vec![StyledSpan::bold("héllo")]);
    }
}
