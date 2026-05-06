use twox_hash::XxHash3_64;

pub fn hash_line(content: &str) -> u64 {
    XxHash3_64::oneshot(content.trim().as_bytes())
}

pub fn hash_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<u64> {
    let mut blank_idx: u64 = 0;
    lines
        .into_iter()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                blank_idx += 1;
                XxHash3_64::oneshot_with_seed(blank_idx, b"")
            } else {
                XxHash3_64::oneshot(trimmed.as_bytes())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_whitespace() {
        assert_eq!(hash_line("foo"), hash_line("  foo  "));
        assert_eq!(hash_line("foo"), hash_line("\tfoo\n"));
    }

    #[test]
    fn distinct_content_distinct_hash() {
        assert_ne!(hash_line("foo"), hash_line("bar"));
    }

    #[test]
    fn adjacent_blanks_differ() {
        let hs = hash_lines(["", "", "x", ""]);
        assert_ne!(hs[0], hs[1]);
        assert_ne!(hs[1], hs[3]);
        assert_ne!(hs[0], hs[3]);
    }

    #[test]
    fn identical_nonblanks_match() {
        let hs = hash_lines(["foo", "bar", "foo"]);
        assert_eq!(hs[0], hs[2]);
        assert_ne!(hs[0], hs[1]);
    }

    #[test]
    fn whitespace_only_lines_treated_as_blank() {
        let hs = hash_lines(["  ", "\t"]);
        assert_ne!(hs[0], hs[1]);
    }
}
