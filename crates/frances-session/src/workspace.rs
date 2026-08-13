//! Workspace resolution: what a `frances [path]` launch opens.
//!
//! A workspace is a collection of directories, VS Code style. A bare
//! directory acts as an implicit single-dir workspace; a regular file
//! is parsed as a workspace file — JSON `{ "dirs": [...] }` with
//! relative entries resolved against the file's parent directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("resolve workspace path {}", path.display())]
    Resolve {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read workspace file {}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse workspace file {}", path.display())]
    ParseFile {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("workspace file {} lists no dirs", path.display())]
    NoDirs { path: PathBuf },
    #[error("workspace dir {} (from {})", dir.display(), path.display())]
    ResolveDir {
        path: PathBuf,
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What the launch path named: a bare directory (implicit single-dir
/// workspace) or a workspace file. Paths are canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSource {
    Dir(PathBuf),
    File(PathBuf),
}

impl WorkspaceSource {
    /// The canonical path that identifies this workspace — what session
    /// metadata records so sessions can later be grouped by workspace.
    pub fn identity_path(&self) -> &Path {
        match self {
            WorkspaceSource::Dir(path) | WorkspaceSource::File(path) => path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub source: WorkspaceSource,
    /// Canonical, non-empty. `dirs[0]` is the primary dir — the cwd
    /// sessions start in.
    dirs: Vec<PathBuf>,
}

/// On-disk workspace-file shape.
#[derive(Deserialize)]
struct WorkspaceFile {
    dirs: Vec<PathBuf>,
}

impl Workspace {
    pub fn open(path: &Path) -> Result<Self, WorkspaceError> {
        let canonical = fs::canonicalize(path).map_err(|source| WorkspaceError::Resolve {
            path: path.to_path_buf(),
            source,
        })?;

        if canonical.is_dir() {
            return Ok(Self {
                source: WorkspaceSource::Dir(canonical.clone()),
                dirs: vec![canonical],
            });
        }

        let text = fs::read_to_string(&canonical).map_err(|source| WorkspaceError::ReadFile {
            path: canonical.clone(),
            source,
        })?;
        let file: WorkspaceFile =
            serde_json::from_str(&text).map_err(|source| WorkspaceError::ParseFile {
                path: canonical.clone(),
                source,
            })?;
        if file.dirs.is_empty() {
            return Err(WorkspaceError::NoDirs { path: canonical });
        }

        // The file's parent is the base for relative entries. A canonical
        // file path always has a parent.
        let base = canonical.parent().unwrap_or(Path::new("/"));
        let dirs = file
            .dirs
            .into_iter()
            .map(|dir| {
                fs::canonicalize(base.join(&dir)).map_err(|source| WorkspaceError::ResolveDir {
                    path: canonical.clone(),
                    dir,
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            source: WorkspaceSource::File(canonical),
            dirs,
        })
    }

    /// The directory sessions start in: the bare dir itself, or the
    /// workspace file's first entry.
    pub fn primary_dir(&self) -> &Path {
        &self.dirs[0]
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}
