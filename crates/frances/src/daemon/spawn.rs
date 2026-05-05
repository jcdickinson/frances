use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tracing::info;

use crate::daemon::client;
use crate::daemon::protocol::PROTOCOL_VERSION;
use crate::session::Session;

const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub async fn ensure_daemon(session: &Session) -> Result<()> {
    // First strike: a daemon is already listening, but its build differs.
    // Stop it and let the spawn path take over.
    if let Ok(version) = client::daemon_version(session).await {
        if version == PROTOCOL_VERSION {
            return Ok(());
        }
        info!(
            daemon = format!("{version:016x}"),
            client = format!("{:016x}", PROTOCOL_VERSION),
            "protocol version mismatch — restarting daemon"
        );
        let _ = client::stop(session, false).await;
        wait_for_down(session, READINESS_TIMEOUT).await?;
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

    // Second strike: a freshly-spawned daemon should match our build. If it
    // doesn't, something on disk is out of sync (e.g. a different binary got
    // exec'd). Don't loop — bail loudly.
    let version = wait_for_ready_and_check(session, READINESS_TIMEOUT).await?;
    if version != PROTOCOL_VERSION {
        return Err(anyhow!(
            "freshly-spawned daemon reports {version:016x} but client is {client:016x} — client binary out of sync with disk",
            client = PROTOCOL_VERSION,
        ));
    }
    Ok(())
}

async fn wait_for_ready_and_check(session: &Session, timeout: Duration) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(version) = client::daemon_version(session).await {
            return Ok(version);
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL).await;
    }
    Err(anyhow!(
        "daemon for session {} did not become ready within {:?}",
        session.id,
        timeout
    ))
}

async fn wait_for_down(session: &Session, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if client::daemon_version(session).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL).await;
    }
    Err(anyhow!(
        "daemon for session {} did not shut down within {:?}",
        session.id,
        timeout
    ))
}

pub fn cleanup_stale_runtime(session: &Session) -> Result<()> {
    if let Ok(contents) = fs::read_to_string(session.pid_path())
        && let Ok(pid) = contents.trim().parse::<u32>()
        && std::path::PathBuf::from(format!("/proc/{pid}")).exists()
    {
        return Ok(());
    }

    client::remove_socket_if_present(&session.control_socket_path())?;
    client::remove_socket_if_present(&session.client_socket_path())?;
    client::remove_socket_if_present(&session.events_socket_path())?;

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
