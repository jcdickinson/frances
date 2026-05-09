use std::path::Path;

use frances_edit::{EditEngine, FakeStore};

use super::EditSession;
use crate::Result;

pub(super) fn lines_of(s: &str) -> Vec<String> {
    s.lines().map(str::to_owned).collect()
}

pub(super) fn fresh_session() -> EditSession<FakeStore> {
    EditSession::new(EditEngine::new(FakeStore::new()))
}

pub(super) fn no_format(_: &Path, draft: &[String]) -> Result<(Vec<String>, i64, u64)> {
    let size: u64 = draft.iter().map(|l| (l.len() + 1) as u64).sum();
    Ok((draft.to_vec(), 200, size))
}
