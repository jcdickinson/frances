use std::fmt::{self, Write};

const MAX_CHARS: usize = 80;
const ELLIPSIS: char = '…';

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Truncated(pub String);

impl Truncated {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for Truncated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.chars().count();
        if count <= MAX_CHARS {
            f.write_str(&self.0)
        } else {
            for c in self.0.chars().take(MAX_CHARS - 1) {
                f.write_char(c)?;
            }
            f.write_char(ELLIPSIS)
        }
    }
}

impl<S: Into<String>> From<S> for Truncated {
    fn from(s: S) -> Self {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_passes_through() {
        let t = Truncated::new("hello");
        assert_eq!(t.to_string(), "hello");
    }

    #[test]
    fn exact_max_passes_through() {
        let s = "a".repeat(MAX_CHARS);
        let t = Truncated::new(s.clone());
        assert_eq!(t.to_string(), s);
    }

    #[test]
    fn over_max_truncates_with_ellipsis() {
        let s = "a".repeat(MAX_CHARS + 50);
        let t = Truncated::new(s);
        let rendered = t.to_string();
        assert_eq!(rendered.chars().count(), MAX_CHARS);
        assert!(rendered.ends_with(ELLIPSIS));
    }

    #[test]
    fn char_boundary_aware() {
        let s = "🦀".repeat(MAX_CHARS + 10);
        let t = Truncated::new(s);
        let rendered = t.to_string();
        assert_eq!(rendered.chars().count(), MAX_CHARS);
        assert!(rendered.ends_with(ELLIPSIS));
    }
}
