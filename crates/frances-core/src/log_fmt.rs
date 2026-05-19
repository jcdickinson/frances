//! Formatting helpers for log/trace output.

use std::borrow::Cow;
use std::fmt;

/// `Display` wrapper that emits at most the last `N` chars of the wrapped
/// string, prefixed with `...` when truncation happens. UTF-8 safe.
///
/// The truncation runs inside `Display::fmt` without allocating: we walk
/// `char_indices().rev().nth(N - 1)` to find the byte boundary of the
/// `N`-th-from-last char, then write the suffix as a borrowed slice.
pub struct Truncated<'a, const N: usize>(pub Cow<'a, str>);

impl<'a, const N: usize> Truncated<'a, N> {
    pub fn new(s: impl Into<Cow<'a, str>>) -> Self {
        Self(s.into())
    }
}

impl<const N: usize> fmt::Display for Truncated<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.as_ref();
        if N == 0 {
            return f.write_str("...");
        }
        match s.char_indices().rev().nth(N - 1) {
            Some((idx, _)) if idx > 0 => {
                f.write_str("...")?;
                f.write_str(&s[idx..])
            }
            _ => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Truncated;

    fn render<const N: usize>(s: &str) -> String {
        format!("{}", Truncated::<N>::new(s))
    }

    #[test]
    fn short_string_passes_through() {
        assert_eq!(render::<100>("hi"), "hi");
    }

    #[test]
    fn exact_length_passes_through() {
        assert_eq!(render::<5>("abcde"), "abcde");
    }

    #[test]
    fn long_string_keeps_tail() {
        assert_eq!(render::<4>("abcdefghij"), "...ghij");
    }

    #[test]
    fn handles_multibyte_chars() {
        // 5 crabs, each 4 bytes in UTF-8. Keep the last 2.
        assert_eq!(render::<2>("🦀🦀🦀🦀🦀"), "...🦀🦀");
    }

    #[test]
    fn zero_cap_emits_ellipsis_only() {
        assert_eq!(render::<0>("anything"), "...");
        assert_eq!(render::<0>(""), "...");
    }

    #[test]
    fn empty_string_stays_empty() {
        assert_eq!(render::<10>(""), "");
    }
}
