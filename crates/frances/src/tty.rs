use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// Hashed identifier for the invoking process's controlling TTY. Used as a
/// session-link filename. Distinct from arbitrary strings to prevent
/// confusion with session ids, paths, etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TtyKey(pub String);

impl TtyKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TtyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TtyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(pub i32);

/// `tty_nr` from `/proc/self/stat` — kernel-encoded device number with a
/// non-standard bit layout, distinct from the file's `rdev` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TtyDeviceNr(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Inode(pub u64);

/// Identifies the invoking process's controlling terminal. Only `key` is
/// read externally (used as a session-link filename); the other fields are
/// retained as forensic context — anything that contributes to `key` should
/// stay reachable so we can debug a stale link.
#[derive(Debug, Clone)]
pub struct TtyIdentity {
    pub key: TtyKey,
    pub tty_path: PathBuf,
    pub session_leader: Pid,
    pub tty_nr: TtyDeviceNr,
    pub dev: DeviceId,
    pub inode: Inode,
    pub rdev: DeviceId,
}

pub fn controlling_tty_key() -> Result<TtyKey> {
    Ok(controlling_tty_identity()?.key)
}

pub fn controlling_tty_identity() -> Result<TtyIdentity> {
    let fd = 0;
    let is_tty = unsafe { libc::isatty(fd) };
    if is_tty != 1 {
        return Err(anyhow!("no controlling TTY available"));
    }

    let tty_path =
        fs::read_link("/proc/self/fd/0").context("failed to resolve controlling tty path")?;
    let metadata = fs::metadata("/proc/self/fd/0").context("failed to stat controlling tty")?;
    let (session_leader, tty_nr) = read_proc_self_stat()?;

    let mut hasher = DefaultHasher::new();
    tty_path.hash(&mut hasher);
    session_leader.0.hash(&mut hasher);
    tty_nr.0.hash(&mut hasher);
    metadata.dev().hash(&mut hasher);
    metadata.ino().hash(&mut hasher);
    metadata.rdev().hash(&mut hasher);
    let key = TtyKey(format!("{:016x}", hasher.finish()));

    Ok(TtyIdentity {
        key,
        tty_path,
        session_leader,
        tty_nr,
        dev: DeviceId(metadata.dev()),
        inode: Inode(metadata.ino()),
        rdev: DeviceId(metadata.rdev()),
    })
}

fn read_proc_self_stat() -> Result<(Pid, TtyDeviceNr)> {
    let stat = fs::read_to_string("/proc/self/stat").context("failed to read /proc/self/stat")?;
    let rparen = stat
        .rfind(')')
        .ok_or_else(|| anyhow!("unexpected /proc/self/stat format"))?;
    let rest = stat
        .get(rparen + 2..)
        .ok_or_else(|| anyhow!("unexpected /proc/self/stat fields"))?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 5 {
        return Err(anyhow!("not enough /proc/self/stat fields"));
    }

    let session_leader = fields[3]
        .parse::<i32>()
        .context("failed to parse session id from /proc/self/stat")?;
    let tty_nr = fields[4]
        .parse::<i64>()
        .context("failed to parse tty_nr from /proc/self/stat")?;

    Ok((Pid(session_leader), TtyDeviceNr(tty_nr)))
}
