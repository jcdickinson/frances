use std::fs;
use std::io;

/// Linux-only: read `/proc/<parent>/task/<parent>/children` and return the
/// list of immediate child PIDs. When bash is blocked in `wait()` for a
/// foreground command, that command appears here as a child.
pub fn list_children(parent: u32) -> io::Result<Vec<u32>> {
    let path = format!("/proc/{parent}/task/{parent}/children");
    let s = fs::read_to_string(path)?;
    Ok(s.split_whitespace()
        .filter_map(|t| t.parse::<u32>().ok())
        .collect())
}

/// `kill(2)` the given PID with `sig`. ESRCH (process already gone) is
/// treated as success — the foreground command may have just finished.
pub fn signal_pid(pid: u32, sig: libc::c_int) -> io::Result<()> {
    let r = unsafe { libc::kill(pid as i32, sig) };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}
