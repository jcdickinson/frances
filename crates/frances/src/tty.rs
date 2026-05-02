use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

#[derive(Debug, Clone)]
pub struct TtyIdentity {
    pub key: String,
    pub tty_path: PathBuf,
    pub session_leader: i32,
    pub tty_nr: i64,
    pub dev: u64,
    pub inode: u64,
    pub rdev: u64,
}

pub fn controlling_tty_key() -> Result<String> {
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
    session_leader.hash(&mut hasher);
    tty_nr.hash(&mut hasher);
    metadata.dev().hash(&mut hasher);
    metadata.ino().hash(&mut hasher);
    metadata.rdev().hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());

    Ok(TtyIdentity {
        key,
        tty_path,
        session_leader,
        tty_nr,
        dev: metadata.dev(),
        inode: metadata.ino(),
        rdev: metadata.rdev(),
    })
}

fn read_proc_self_stat() -> Result<(i32, i64)> {
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

    Ok((session_leader, tty_nr))
}
