use std::fmt::Write;
use std::path::Path;

const NONCE_BYTES: usize = 8;

pub fn make_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    let mut s = String::with_capacity(NONCE_BYTES * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

#[derive(Debug, Clone)]
pub struct Sentinel {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentinelMatch {
    pub output_len: usize,
    pub consumed: usize,
    pub exit_code: i32,
}

impl Sentinel {
    pub fn new(nonce: &str) -> Self {
        Self {
            prefix: format!("__F_{nonce}_").into_bytes(),
            suffix: b"__\n".to_vec(),
        }
    }

    /// Upper bound (in bytes) on a complete sentinel match, including
    /// the leading `\n`. Streaming consumers use this to know how many
    /// trailing buffer bytes must be held back from "safe" delivery so
    /// they don't ship the start of an in-progress sentinel as output.
    /// The digit count is bounded by the longest `i32` decimal
    /// representation (`-2147483648`, 11 bytes).
    pub fn max_match_len(&self) -> usize {
        const MAX_EXIT_DIGITS: usize = 11;
        1 + self.prefix.len() + MAX_EXIT_DIGITS + self.suffix.len()
    }

    pub fn find(&self, buf: &[u8]) -> Option<SentinelMatch> {
        let mut i = 0;
        while i + 1 + self.prefix.len() <= buf.len() {
            if buf[i] != b'\n' || !buf[i + 1..].starts_with(&self.prefix) {
                i += 1;
                continue;
            }
            let digits_start = i + 1 + self.prefix.len();
            let mut digits_end = digits_start;
            while digits_end < buf.len() && buf[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end == digits_start {
                i += 1;
                continue;
            }
            if !buf[digits_end..].starts_with(&self.suffix) {
                return None;
            }
            let digits = std::str::from_utf8(&buf[digits_start..digits_end]).ok()?;
            let exit_code: i32 = digits.parse().ok()?;
            let consumed = digits_end + self.suffix.len();
            return Some(SentinelMatch {
                output_len: i,
                consumed,
                exit_code,
            });
        }
        None
    }
}

pub fn wrapper_bytes(cmd_path: &Path, nonce: &str) -> Vec<u8> {
    let quoted = shell_single_quote(&cmd_path.to_string_lossy());
    format!(". {quoted}\nprintf '\\n__F_{nonce}_%d__\\n' \"$?\"\n").into_bytes()
}

pub fn handshake_bytes(nonce: &str) -> Vec<u8> {
    format!("exec 2>&1\nprintf '\\n__F_{nonce}_%d__\\n' \"$?\"\n").into_bytes()
}

pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn nonce_shape() {
        let n = make_nonce();
        assert_eq!(n.len(), 16);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn nonce_changes() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            seen.insert(make_nonce());
        }
        assert!(seen.len() > 1, "nonce never changed across 32 calls");
    }

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_single_quote("foo"), "'foo'");
    }

    #[test]
    fn shell_quote_with_apostrophe() {
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn wrapper_uses_literal_nonce() {
        let bytes = wrapper_bytes(&PathBuf::from("/tmp/x/cmd.sh"), "deadbeef00112233");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains(". '/tmp/x/cmd.sh'"));
        assert!(s.contains("__F_deadbeef00112233_%d__"));
        assert!(!s.contains("$__F_NONCE"));
    }

    #[test]
    fn handshake_uses_literal_nonce() {
        let bytes = handshake_bytes("cafebabe12345678");
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("exec 2>&1\n"));
        assert!(s.contains("__F_cafebabe12345678_%d__"));
    }

    #[test]
    fn sentinel_find_single_chunk() {
        let s = Sentinel::new("abc");
        let buf = b"hello\n\n__F_abc_0__\n";
        let m = s.find(buf).unwrap();
        assert_eq!(m.exit_code, 0);
        assert_eq!(&buf[..m.output_len], b"hello\n");
        assert_eq!(m.consumed, buf.len());
    }

    #[test]
    fn sentinel_find_no_trailing_newline_in_output() {
        let s = Sentinel::new("abc");
        let buf = b"hello\n__F_abc_42__\n";
        let m = s.find(buf).unwrap();
        assert_eq!(m.exit_code, 42);
        assert_eq!(&buf[..m.output_len], b"hello");
    }

    #[test]
    fn sentinel_no_match() {
        let s = Sentinel::new("abc");
        assert!(s.find(b"hello world\n").is_none());
    }

    #[test]
    fn sentinel_partial_match_returns_none() {
        let s = Sentinel::new("abc");
        // Has the prefix and digits but no closing __\n yet.
        assert!(s.find(b"\n__F_abc_12").is_none());
        assert!(s.find(b"\n__F_abc_12_").is_none());
        assert!(s.find(b"\n__F_abc_12__").is_none());
    }

    #[test]
    fn sentinel_ignores_wrong_nonce() {
        let s = Sentinel::new("abc");
        assert!(s.find(b"\n__F_xyz_0__\n").is_none());
    }

    #[test]
    fn sentinel_finds_after_junk() {
        let s = Sentinel::new("abc");
        let buf = b"junk1\njunk2 __F_abc_0__\nstill garbage\n__F_abc_7__\n";
        let m = s.find(buf).unwrap();
        // First real candidate is `\n__F_abc_0__\n` after "junk1".
        // "junk1\n__F_abc_0__\n" wait — that's the actual structure, "junk2 __F_abc_0__" lacks the leading \n adjacency.
        // Let me re-read: "junk1" + "\n" + "junk2 __F_abc_0__" + "\n" + "still garbage" + "\n__F_abc_7__\n".
        // The first \n followed immediately by __F_abc_ is right before "still garbage"? No — "\njunk2..." has \n followed by 'j', not __F_.
        // Then "still garbage\n__F_abc_7__\n" — the \n before __F_abc_7__ is the match.
        assert_eq!(m.exit_code, 7);
    }

    #[test]
    fn sentinel_handles_digits_only() {
        let s = Sentinel::new("abc");
        // Multi-digit exit code.
        let m = s.find(b"\n__F_abc_137__\n").unwrap();
        assert_eq!(m.exit_code, 137);
    }
}
