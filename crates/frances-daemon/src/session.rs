use std::env;
use std::fs;
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Result;
use crate::tty::TtyKey;

const METADATA_FILE: &str = "metadata.bin";
const SESSION_DIR_MODE: u32 = 0o700;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("HOME is not set")]
    HomeNotSet,
    #[error("session metadata id mismatch for {requested}: file says {found}")]
    MetadataIdMismatch { requested: String, found: String },
    #[error("invalid tty link target for {tty_key}: {}", target.display())]
    InvalidTtyLinkTarget { tty_key: String, target: PathBuf },
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
    #[error("read tty link {tty_key}: {source}")]
    ReadTtyLink {
        tty_key: String,
        #[source]
        source: std::io::Error,
    },
    #[error("create tty link for {tty_key}: {source}")]
    CreateTtyLink {
        tty_key: String,
        #[source]
        source: std::io::Error,
    },
    #[error("remove tty link {tty_key}: {source}")]
    RemoveTtyLink {
        tty_key: String,
        #[source]
        source: std::io::Error,
    },
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
    pub cwd: Option<PathBuf>,
    pub reserved: Option<String>,
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
        create_private_dir(&self.tty_links_root())?;
        Ok(())
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.state_root.join("sessions")
    }

    pub fn runtime_sessions_root(&self) -> PathBuf {
        self.runtime_root.join("sessions")
    }

    pub fn tty_links_root(&self) -> PathBuf {
        self.runtime_root.join("tty-links")
    }

    pub fn create_session(&self, cwd: Option<PathBuf>) -> Result<Session> {
        let id = generate_session_id();
        let dir = self.sessions_root().join(&id);
        let runtime_dir = self.runtime_sessions_root().join(&id);

        create_private_dir(&dir)?;
        create_private_dir(&runtime_dir)?;

        let meta = SessionMeta {
            version: 1,
            id: id.clone(),
            created: now_unix_secs(),
            cwd,
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

    pub fn resolve_or_create_for_tty(
        &self,
        tty_key: &TtyKey,
        cwd: Option<PathBuf>,
    ) -> Result<Session> {
        if let Some(session) = self.resolve_tty_link(tty_key)? {
            return Ok(session);
        }

        let session = self.create_session(cwd)?;
        self.link_tty(tty_key, &session)?;
        Ok(session)
    }

    pub fn resolve_tty_link(&self, tty_key: &TtyKey) -> Result<Option<Session>> {
        let link_path = self.tty_link_path(tty_key);
        if !link_path.exists() {
            return Ok(None);
        }

        let target = match fs::read_link(&link_path) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SessionError::ReadTtyLink {
                    tty_key: tty_key.to_string(),
                    source,
                }
                .into());
            }
        };

        if !target.exists() {
            let _ = fs::remove_file(&link_path);
            return Ok(None);
        }

        let session_id = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| SessionError::InvalidTtyLinkTarget {
                tty_key: tty_key.to_string(),
                target: target.clone(),
            })?;

        match self.load_session(session_id) {
            Ok(session) => Ok(Some(session)),
            Err(_) => {
                let _ = fs::remove_file(&link_path);
                Ok(None)
            }
        }
    }

    pub fn link_tty(&self, tty_key: &TtyKey, session: &Session) -> Result<()> {
        let link_path = self.tty_link_path(tty_key);
        if link_path.exists() {
            let _ = fs::remove_file(&link_path);
        }
        symlink(&session.dir, &link_path).map_err(|source| SessionError::CreateTtyLink {
            tty_key: tty_key.to_string(),
            source,
        })?;
        Ok(())
    }

    pub fn unlink_tty(&self, tty_key: &TtyKey) -> Result<bool> {
        let link_path = self.tty_link_path(tty_key);
        match fs::remove_file(&link_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SessionError::RemoveTtyLink {
                tty_key: tty_key.to_string(),
                source,
            }
            .into()),
        }
    }

    pub fn tty_link_path(&self, tty_key: &TtyKey) -> PathBuf {
        self.tty_links_root().join(tty_key.as_str())
    }
}

impl Session {
    pub fn control_socket_path(&self) -> PathBuf {
        self.runtime_dir.join("control.sock")
    }

    pub fn client_socket_path(&self) -> PathBuf {
        self.runtime_dir.join("client.sock")
    }

    pub fn events_socket_path(&self) -> PathBuf {
        self.runtime_dir.join("events.sock")
    }

    pub fn pid_path(&self) -> PathBuf {
        self.runtime_dir.join("daemon.pid")
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.dir.join(METADATA_FILE)
    }

    pub fn database_path(&self) -> PathBuf {
        self.dir.join("frances.db")
    }
}

fn generate_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", nanos, std::process::id())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

