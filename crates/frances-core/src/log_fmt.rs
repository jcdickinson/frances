//! Formatting helpers for log/trace output.

use std::borrow::Cow;
use std::fmt;

const ELLIPSIS: &str = "…";

/// `Display` wrapper that emits at most `N` chars of the wrapped string, with an
/// ellipsis marking where content was dropped. UTF-8 safe; no allocation.
///
/// `KEEP_HEAD` selects which end survives:
/// - `false` (default) keeps the trailing `N` chars and prepends the ellipsis —
///   used for trace logs, where the most recent bytes matter most.
/// - `true` keeps the leading `N` chars and appends the ellipsis — used for
///   error messages like edit content-mismatch, where the start matters most.
///
/// A `Cow<'static, str>` is owned, `Eq`, and `Hash`, so the `KEEP_HEAD` form can
/// be stored as a struct field (e.g. inside an error variant).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Truncated<'a, const N: usize, const KEEP_HEAD: bool = false>(pub Cow<'a, str>);

impl<'a, const N: usize, const KEEP_HEAD: bool> Truncated<'a, N, KEEP_HEAD> {
    pub fn new(s: impl Into<Cow<'a, str>>) -> Self {
        Self(s.into())
    }
}

impl<const N: usize, const KEEP_HEAD: bool> fmt::Display for Truncated<'_, N, KEEP_HEAD> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.as_ref();
        if KEEP_HEAD {
            match s.char_indices().nth(N) {
                Some((idx, _)) => {
                    f.write_str(&s[..idx])?;
                    f.write_str(ELLIPSIS)
                }
                None => f.write_str(s),
            }
        } else {
            if N == 0 {
                return f.write_str(ELLIPSIS);
            }
            match s.char_indices().rev().nth(N - 1) {
                Some((idx, _)) if idx > 0 => {
                    f.write_str(ELLIPSIS)?;
                    f.write_str(&s[idx..])
                }
                _ => f.write_str(s),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Truncated;

    fn tail<const N: usize>(s: &str) -> String {
        format!("{}", Truncated::<N>::new(s))
    }

    fn head<const N: usize>(s: &str) -> String {
        format!("{}", Truncated::<N, true>::new(s))
    }

    #[test]
    fn short_string_passes_through() {
        assert_eq!(tail::<100>("hi"), "hi");
        assert_eq!(head::<100>("hi"), "hi");
    }

    #[test]
    fn exact_length_passes_through() {
        assert_eq!(tail::<5>("abcde"), "abcde");
        assert_eq!(head::<5>("abcde"), "abcde");
    }

    #[test]
    fn tail_keeps_end() {
        assert_eq!(tail::<4>("abcdefghij"), "…ghij");
    }

    #[test]
    fn head_keeps_start() {
        assert_eq!(head::<4>("abcdefghij"), "abcd…");
    }

    #[test]
    fn handles_multibyte_chars() {
        // 5 crabs, each 4 bytes in UTF-8.
        assert_eq!(tail::<2>("🦀🦀🦀🦀🦀"), "…🦀🦀");
        assert_eq!(head::<2>("🦀🦀🦀🦀🦀"), "🦀🦀…");
    }

    #[test]
    fn zero_cap_emits_ellipsis_only() {
        assert_eq!(tail::<0>("anything"), "…");
        assert_eq!(tail::<0>(""), "…");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(tail::<10>(""), "");
        assert_eq!(head::<10>(""), "");
    }
}
