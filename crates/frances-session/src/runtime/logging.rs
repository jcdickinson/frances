use std::fs;
use std::sync::Mutex;

use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::Result;
use crate::session::Session;

use super::RuntimeError;

/// Default to warn for the world; raise frances/* crates to trace.
fn default_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "warn,frances=trace,frances_session=trace,frances_edit=trace,frances_anchors=trace,frances_config=trace",
        )
    })
}

/// Wire tracing to write into `session.dir/frances.log`, and to stderr for
/// foreground/dev runs (detached launches null stderr, so it's silent there).
pub fn install_logging(session: &Session) -> Result<()> {
    let main_path = session.dir.join("frances.log");
    let main_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&main_path)
        .map_err(|source| RuntimeError::OpenSessionLog {
            path: main_path.clone(),
            source,
        })?;
    let main_writer = Mutex::new(main_file);

    let main_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(main_writer)
        .with_filter(default_filter());

    let console_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(default_filter());

    tracing_subscriber::registry()
        .with(main_layer)
        .with(console_layer)
        .try_init()
        .map_err(RuntimeError::InstallSubscriber)?;

    // Dump each outgoing LLM request to `<session.dir>/request.json` for
    // debugging the exact payload (messages + system/instructions + tools).
    frances_llm::providers::genai::set_request_dump_dir(session.dir.clone());

    info!(session_id = %session.id, log = %main_path.display(), "session runtime logging installed");
    Ok(())
}
