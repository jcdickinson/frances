use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use frances_core::now_unix_secs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::Result;
use crate::workspace::Workspace;

const METADATA_FILE: &str = "metadata.bin";
const SESSION_DIR_MODE: u32 = 0o700;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("HOME is not set")]
    HomeNotSet,
    #[error("session metadata id mismatch for {requested}: file says {found}")]
    MetadataIdMismatch { requested: String, found: String },
    #[error("create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("set permissions on {path}: {source}")]
    SetPermissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write metadata {path}: {source}")]
    WriteMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read metadata {path}: {source}")]
    ReadMetadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("encode metadata: {0}")]
    EncodeMetadata(#[from] bincode::error::EncodeError),
    #[error("decode metadata: {0}")]
    DecodeMetadata(#[from] bincode::error::DecodeError),
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub state_root: PathBuf,
    pub runtime_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub id: String,
    pub created: u64,
    /// The workspace's primary dir at creation time — where the session
    /// started.
    pub cwd: PathBuf,
    /// Canonical path of the dir or workspace file the session was
    /// opened on. Lets sessions be grouped by workspace later (MRU,
    /// pickers) — the workspace itself is re-read from this path, not
    /// stored here.
    pub workspace_source: PathBuf,
    pub workflow: Option<SessionWorkflow>,
    /// Human-readable session title. Set by the active workflow via
    /// `setTitle`; `None` until one is set.
    pub title: Option<String>,
    pub reserved: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionWorkflow {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub paths: Paths,
    pub id: String,
    pub dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub meta: SessionMeta,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let state_root = match env::var_os("XDG_STATE_HOME") {
            Some(value) => PathBuf::from(value).join("frances"),
            None => {
                let home = env::var_os("HOME").ok_or(SessionError::HomeNotSet)?;
                PathBuf::from(home).join(".local/state/frances")
            }
        };

        // xdg's `runtime_dir` field is the raw `Option<PathBuf>` view of
        // `$XDG_RUNTIME_DIR` — no default and no sanity checks. The spec
        // doesn't define a fallback (the standard explicitly leaves it to
        // applications), so we keep the previous `/tmp/frances-<uid>`
        // default for setups without a session manager.
        let runtime_root = match xdg::BaseDirectories::new().runtime_dir {
            Some(dir) => dir.join("frances"),
            None => PathBuf::from(format!("/tmp/frances-{}", current_uid())),
        };

        let paths = Self {
            state_root,
            runtime_root,
        };
        paths.ensure_layout()?;
        Ok(paths)
    }

    pub fn ensure_layout(&self) -> Result<()> {
        create_private_dir(&self.state_root)?;
        create_private_dir(&self.sessions_root())?;
        create_private_dir(&self.runtime_root)?;
        create_private_dir(&self.runtime_sessions_root())?;
        Ok(())
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.state_root.join("sessions")
    }

    pub fn runtime_sessions_root(&self) -> PathBuf {
        self.runtime_root.join("sessions")
    }

    pub fn create_session(&self, workspace: &Workspace) -> Result<Session> {
        let id = generate_session_id();
        let dir = self.sessions_root().join(&id);
        let runtime_dir = self.runtime_sessions_root().join(&id);

        create_private_dir(&dir)?;
        create_private_dir(&runtime_dir)?;

        let meta = SessionMeta {
            version: 1,
            id: id.clone(),
            created: now_unix_secs(),
            cwd: workspace.primary_dir().to_path_buf(),
            workspace_source: workspace.source.identity_path().to_path_buf(),
            workflow: None,
            title: None,
            reserved: None,
        };

        write_metadata(&dir.join(METADATA_FILE), &meta)?;

        Ok(Session {
            paths: self.clone(),
            id,
            dir,
            runtime_dir,
            meta,
        })
    }

    pub fn load_session(&self, id: &str) -> Result<Session> {
        let dir = self.sessions_root().join(id);
        let runtime_dir = self.runtime_sessions_root().join(id);
        let meta = read_metadata(&dir.join(METADATA_FILE))?;

        if meta.id != id {
            return Err(SessionError::MetadataIdMismatch {
                requested: id.to_string(),
                found: meta.id,
            }
            .into());
        }

        create_private_dir(&runtime_dir)?;

        Ok(Session {
            paths: self.clone(),
            id: id.to_string(),
            dir,
            runtime_dir,
            meta,
        })
    }
}

impl Session {
    pub fn metadata_path(&self) -> PathBuf {
        self.dir.join(METADATA_FILE)
    }

    pub fn database_path(&self) -> PathBuf {
        self.dir.join("frances.db")
    }

    /// Read-modify-write the metadata file. Rereads from disk (rather
    /// than cloning the boot-time `self.meta` snapshot) so one field's
    /// update can't clobber another's earlier write. All post-boot
    /// writes happen on the workflow driver task, so there is no
    /// concurrent-writer race to guard.
    pub fn update_meta(&self, update: impl FnOnce(&mut SessionMeta)) -> Result<()> {
        let mut meta = read_metadata(&self.metadata_path())?;
        update(&mut meta);
        write_metadata(&self.metadata_path(), &meta)
    }

    pub fn write_workflow(&self, workflow: SessionWorkflow) -> Result<()> {
        self.update_meta(|meta| meta.workflow = Some(workflow))
    }

    pub fn write_title(&self, title: Option<String>) -> Result<()> {
        self.update_meta(|meta| meta.title = title)
    }
}

fn generate_session_id() -> String {
    Uuid::new_v4().to_string()
}

fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| SessionError::CreateDir {
        path: path.to_path_buf(),
        source,
    })?;
    let permissions = fs::Permissions::from_mode(SESSION_DIR_MODE);
    fs::set_permissions(path, permissions).map_err(|source| SessionError::SetPermissions {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn write_metadata(path: &Path, meta: &SessionMeta) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(meta, bincode::config::standard())
        .map_err(SessionError::EncodeMetadata)?;
    fs::write(path, bytes).map_err(|source| SessionError::WriteMetadata {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn read_metadata(path: &Path) -> Result<SessionMeta> {
    let bytes = fs::read(path).map_err(|source| SessionError::ReadMetadata {
        path: path.to_path_buf(),
        source,
    })?;
    let (meta, _) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
        .map_err(SessionError::DecodeMetadata)?;
    Ok(meta)
}
