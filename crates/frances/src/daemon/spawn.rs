use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::daemon::client;
use crate::session::Session;

const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub fn ensure_daemon(session: &Session) -> Result<()> {
    if client::ping(session).is_ok() {
        return Ok(());
    }

    cleanup_stale_runtime(session)?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    Command::new(current_exe)
        .arg("--daemon")
        .arg(&session.id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn daemon for session {}", session.id))?;

    wait_for_ready(session, READINESS_TIMEOUT)
}

pub fn wait_for_ready(session: &Session, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client::ping(session).is_ok() {
            return Ok(());
        }
        thread::sleep(READINESS_POLL_INTERVAL);
    }

    Err(anyhow!(
        "daemon for session {} did not become ready within {:?}",
        session.id,
        timeout
    ))
}

pub fn cleanup_stale_runtime(session: &Session) -> Result<()> {
    if let Ok(contents) = fs::read_to_string(session.pid_path()) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
            if proc_path.exists() {
                return Ok(());
            }
        }
    }

    client::remove_socket_if_present(&session.control_socket_path())?;
    client::remove_socket_if_present(&session.client_socket_path())?;

    match fs::remove_file(session.pid_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed removing stale pid file for {}", session.id));
        }
    }

    Ok(())
}
