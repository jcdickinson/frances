//! Workspace resolution: what a `frances [path]` launch opens.
//!
//! A workspace is a collection of directories, VS Code style. A bare
//! directory acts as an implicit single-dir workspace; a regular file
//! is parsed as a workspace file — TOML `dirs = ["a", "b"]` with
//! relative entries resolved against the file's parent directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

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
        source: toml::de::Error,
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
    #[error("serialize workspace file {}", path.display())]
    SerializeFile {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("write workspace file {}", path.display())]
    WriteFile {
        path: PathBuf,
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
    /// Stable identity used to group sessions. Read from the workspace
    /// file; a bare dir (or a file without an `id`) gets a fresh one,
    /// which `save` then persists — sessions created before the save
    /// already carry it, so saving links them retroactively.
    pub id: Uuid,
    /// Canonical, non-empty. `dirs[0]` is the primary dir — the cwd
    /// sessions start in.
    dirs: Vec<PathBuf>,
}

/// On-disk workspace-file shape.
#[derive(Serialize, Deserialize)]
struct WorkspaceFile {
    id: Option<Uuid>,
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
                id: Uuid::new_v4(),
                dirs: vec![canonical],
            });
        }

        let text = fs::read_to_string(&canonical).map_err(|source| WorkspaceError::ReadFile {
            path: canonical.clone(),
            source,
        })?;
        let file: WorkspaceFile =
            toml::from_str(&text).map_err(|source| WorkspaceError::ParseFile {
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
            id: file.id.unwrap_or_else(Uuid::new_v4),
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

    /// Write this workspace as a workspace file. Dirs are canonical, so
    /// they're written absolute.
    pub fn save(&self, path: &Path) -> Result<(), WorkspaceError> {
        let file = WorkspaceFile {
            id: Some(self.id),
            dirs: self.dirs.clone(),
        };
        let text = toml::to_string(&file).map_err(|source| WorkspaceError::SerializeFile {
            path: path.to_path_buf(),
            source,
        })?;
        fs::write(path, text).map_err(|source| WorkspaceError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
    }
}
