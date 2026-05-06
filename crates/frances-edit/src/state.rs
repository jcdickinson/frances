use std::path::PathBuf;

use twox_hash::XxHash3_64;

use crate::anchor::Anchor;

#[derive(Clone, Debug)]
pub struct FileAnchorState {
    pub path: PathBuf,
    pub mtime_ns: i64,
    pub size: u64,
    pub content_digest: u64,
    pub lines: Vec<LineEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineEntry {
    pub hash: u64,
    pub anchor: Anchor,
}

impl FileAnchorState {
    pub fn line_hashes(&self) -> Vec<u64> {
        self.lines.iter().map(|le| le.hash).collect()
    }

    pub fn anchor_at(&self, line_no: u32) -> Option<&Anchor> {
        self.lines.get(line_no as usize).map(|le| &le.anchor)
    }

    pub fn find_anchor(&self, anchor: &Anchor) -> Option<u32> {
        self.lines
            .iter()
            .position(|le| &le.anchor == anchor)
            .map(|i| i as u32)
    }
}

pub fn content_digest(line_hashes: &[u64]) -> u64 {
    let bytes: Vec<u8> = line_hashes.iter().flat_map(|h| h.to_le_bytes()).collect();
    XxHash3_64::oneshot(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_digest_stable() {
        assert_eq!(content_digest(&[]), content_digest(&[]));
    }

    #[test]
    fn digest_distinguishes_order() {
        assert_ne!(content_digest(&[1, 2, 3]), content_digest(&[3, 2, 1]));
    }

    #[test]
    fn digest_distinguishes_content() {
        assert_ne!(content_digest(&[1, 2]), content_digest(&[1, 3]));
    }
}
