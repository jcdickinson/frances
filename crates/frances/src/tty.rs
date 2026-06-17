use std::collections::hash_map::DefaultHasher;
use std::ffi::CStr;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use frances_session::tty::TtyKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(pub libc::pid_t);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId(pub libc::dev_t);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Inode(pub libc::ino_t);

/// Unix controlling-terminal identity used to link this TTY to a Frances session.
///
/// The key is derived from the terminal path, terminal session id, and device
/// metadata. The remaining fields are kept so stale session links can be
/// inspected without reverse-engineering the hash input.
#[derive(Debug, Clone)]
#[expect(
    dead_code,
    reason = "forensic context for debugging stale TTY session links; see struct doc"
)]
pub struct TtyIdentity {
    pub key: TtyKey,
    pub tty_path: PathBuf,
    pub session_leader: Pid,
    pub dev: DeviceId,
    pub inode: Inode,
    pub rdev: DeviceId,
}

pub fn controlling_tty_key() -> Result<TtyKey> {
    Ok(controlling_tty_identity()?.key)
}

pub fn controlling_tty_identity() -> Result<TtyIdentity> {
    let tty = File::open("/dev/tty").context("failed to open controlling tty at /dev/tty")?;
    let fd = tty.as_raw_fd();

    let tty_path = tty_name(fd).context("failed to resolve controlling tty path")?;
    let stat = fstat(fd).context("failed to stat controlling tty")?;
    let session_leader = tcgetsid(fd).context("failed to get controlling tty session id")?;

    let mut hasher = DefaultHasher::new();
    tty_path.hash(&mut hasher);
    session_leader.0.hash(&mut hasher);
    stat.st_dev.hash(&mut hasher);
    stat.st_ino.hash(&mut hasher);
    stat.st_rdev.hash(&mut hasher);
    let key = TtyKey(format!("{:016x}", hasher.finish()));

    Ok(TtyIdentity {
        key,
        tty_path,
        session_leader,
        dev: DeviceId(stat.st_dev),
        inode: Inode(stat.st_ino),
        rdev: DeviceId(stat.st_rdev),
    })
}

fn fstat(fd: RawFd) -> Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat.as_mut_ptr()` points to valid writable storage for one
    // `libc::stat`, and `fstat` initializes it on success. `fd` is borrowed from
    // an open `File` owned by the caller while this function runs.
    let result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("fstat failed");
    }

    // SAFETY: `fstat` returned success, so the `libc::stat` storage has been
    // fully initialized by the OS.
    Ok(unsafe { stat.assume_init() })
}

fn tty_name(fd: RawFd) -> Result<PathBuf> {
    let mut buffer = vec![0; 4096];
    // SAFETY: `buffer` is valid writable storage for `buffer.len()` bytes.
    // `ttyname_r` writes a nul-terminated path on success. `fd` is borrowed from
    // an open `File` owned by the caller while this function runs.
    let result = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result)).context("ttyname_r failed");
    }

    // SAFETY: `ttyname_r` returned success, so `buffer` starts with a valid
    // nul-terminated C string written by libc.
    let name = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Ok(std::ffi::OsStr::from_bytes(name.to_bytes()).into())
}

fn tcgetsid(fd: RawFd) -> Result<Pid> {
    // SAFETY: `tcgetsid` only reads kernel state for the supplied file
    // descriptor. `fd` is borrowed from an open `File` owned by the caller while
    // this function runs.
    let session_id = unsafe { libc::tcgetsid(fd) };
    if session_id == -1 {
        return Err(std::io::Error::last_os_error()).context("tcgetsid failed");
    }

    Ok(Pid(session_id))
}
