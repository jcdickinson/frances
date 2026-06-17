use std::io;

/// `kill(2)` the negative process-group id with `sig`. ESRCH (process group
/// already gone) is treated as success — the invocation may have just
/// finished.
pub fn signal_pgid(pgid: u32, sig: libc::c_int) -> io::Result<()> {
    let r = unsafe { libc::kill(-(pgid as i32), sig) };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}
