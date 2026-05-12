use std::io;
use std::path::Path;

use crate::{EditEngine, FakeStore};

use super::EditSession;

pub fn lines_of(s: &str) -> Vec<String> {
    s.lines().map(str::to_owned).collect()
}

pub fn fresh_session() -> EditSession<FakeStore> {
    EditSession::new(EditEngine::new(FakeStore::new()))
}

pub fn no_format(_: &Path, draft: &[String]) -> io::Result<(Vec<String>, i64, u64)> {
    let size: u64 = draft.iter().map(|l| (l.len() + 1) as u64).sum();
    Ok((draft.to_vec(), 200, size))
}
