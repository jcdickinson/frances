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

/// Wire tracing to write into `session.dir/frances.log`. Stdout / stderr
/// stay attached to the terminal so the in-process TUI can render
/// without being trampled by log writes.
///
/// When the environment variable `TUI_TRACE=1` is set, a second layer
/// also writes to `session.dir/tui.log` with `frances_tui=trace`. The
/// two logs are independent — the main log only captures `frances_tui`
/// at the default level (warn) unless the user explicitly raises it
/// via `RUST_LOG`.
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

    // Default to warn for the world; raise frances/* crates to trace.
    // `frances_tui` is held at `warn` explicitly — `EnvFilter` matches by
    // raw prefix, so `frances=trace` would otherwise also raise all
    // `frances_tui::*` events; those go exclusively to `tui.log` below.
    let main_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "warn,frances=trace,frances_session=trace,frances_edit=trace,frances_anchors=trace,frances_config=trace,frances_tui=warn",
        )
    });
    let main_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(main_writer)
        .with_filter(main_filter);

    let tui_layer = if env_flag("TUI_TRACE") {
        let tui_path = session.dir.join("tui.log");
        let tui_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tui_path)
            .map_err(|source| RuntimeError::OpenSessionLog {
                path: tui_path.clone(),
                source,
            })?;
        let tui_writer = Mutex::new(tui_file);
        let tui_filter = EnvFilter::new("frances_tui=trace");
        Some(
            fmt::layer()
                .with_ansi(false)
                .with_writer(tui_writer)
                .with_filter(tui_filter),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(main_layer)
        .with(tui_layer)
        .try_init()
        .map_err(RuntimeError::InstallSubscriber)?;

    // Dump each outgoing LLM request to `<session.dir>/request.json` for
    // debugging the exact payload (messages + system/instructions + tools).
    frances_llm::providers::genai::set_request_dump_dir(session.dir.clone());

    info!(session_id = %session.id, log = %main_path.display(), "session runtime logging installed");
    if env_flag("TUI_TRACE") {
        info!(
            tui_log = %session.dir.join("tui.log").display(),
            "TUI_TRACE active; frances_tui traces routed to tui.log",
        );
    }
    Ok(())
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}
