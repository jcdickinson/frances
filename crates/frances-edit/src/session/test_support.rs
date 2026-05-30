use std::io;
use std::path::Path;

use crate::{EditEngine, FakeStore, WriteMode};

use super::EditSession;

pub fn lines_of(s: &str) -> Vec<String> {
    s.lines().map(str::to_owned).collect()
}

pub fn fresh_session() -> EditSession<FakeStore> {
    EditSession::new(EditEngine::new(FakeStore::new()))
}

pub fn no_format(
    path: &Path,
    draft: &[String],
    mode: WriteMode,
) -> io::Result<(Vec<String>, i64, u64)> {
    // Mirror the real drafter's `create_new` atomicity so the apply_new
    // "already exists" path stays exercised under the fake writer.
    if mode == WriteMode::CreateNew && path.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
    }
    let size: u64 = draft.iter().map(|l| (l.len() + 1) as u64).sum();
    Ok((draft.to_vec(), 200, size))
}
