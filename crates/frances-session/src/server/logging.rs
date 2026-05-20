use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;

use tracing::info;

use crate::Result;
use crate::session::Session;

use super::ServerError;

pub fn install_logging(session: &Session) -> Result<()> {
    let log_path = session.dir.join("daemon.log");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| ServerError::OpenDaemonLog {
            path: log_path.clone(),
            source,
        })?;

    let fd = file.as_raw_fd();
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) < 0 {
            return Err(ServerError::Dup2Stdout(io::Error::last_os_error()).into());
        }
        if libc::dup2(fd, libc::STDERR_FILENO) < 0 {
            return Err(ServerError::Dup2Stderr(io::Error::last_os_error()).into());
        }
    }
    drop(file);

    // Default to warn for the world; raise frances/frances-edit/frances-anchors
    // /frances-config to trace so we can see our own logs without drowning in
    // turso/hyper/reqwest internals. Overridable via RUST_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "warn,frances=trace,frances_session=trace,frances_edit=trace,frances_anchors=trace,frances_config=trace",
        )
    });
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(io::stderr)
        .finish()
        .try_init()
        .map_err(ServerError::InstallSubscriber)?;

    info!(session_id = %session.id, log = %log_path.display(), "daemon logging installed");
    Ok(())
}
