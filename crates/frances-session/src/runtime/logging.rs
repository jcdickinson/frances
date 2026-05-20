use std::fs;
use std::sync::Mutex;

use tracing::info;

use crate::Result;
use crate::session::Session;

use super::RuntimeError;

/// Wire tracing to write into `session.dir/frances.log`. Stdout / stderr
/// stay attached to the terminal so the in-process TUI can render
/// without being trampled by log writes.
pub fn install_logging(session: &Session) -> Result<()> {
    let log_path = session.dir.join("frances.log");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| RuntimeError::OpenSessionLog {
            path: log_path.clone(),
            source,
        })?;

    let writer = Mutex::new(file);

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
        .with_writer(writer)
        .finish()
        .try_init()
        .map_err(RuntimeError::InstallSubscriber)?;

    info!(session_id = %session.id, log = %log_path.display(), "session runtime logging installed");
    Ok(())
}
