mod tty;
mod tui;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use frances_session::context::InvocationContext;
use frances_session::runtime::{SessionRuntime, install_logging};
use frances_session::session::Paths;
use frances_session::store;
use tracing::debug;

use crate::ui::App;

#[derive(Debug, Parser)]
#[command(name = "frances")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Unlink the current TTY's session so the next invocation starts
    /// a fresh session. The old session's state on disk is left intact;
    /// only the TTY → session link is removed.
    New,
}

#[tokio::main]
async fn main() {
    if let Err(error) = real_main().await {
        eprintln!("frances: {error:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();

    let tty_key = tty::controlling_tty_key()?;
    let paths = Paths::discover()?;

    if matches!(cli.command, Some(Command::New)) && paths.resolve_tty_link(&tty_key)?.is_some() {
        paths.unlink_tty(&tty_key)?;
    }

    let invocation = InvocationContext::capture(Some(tty_key.clone()));
    let session = paths.resolve_or_create_for_tty(&tty_key, invocation.process.cwd.clone())?;

    install_logging(&session)?;
    let db = store::open(&session).await?;
    let (runtime, events_rx) = SessionRuntime::start(session.clone(), db, invocation).await?;
    runtime.replay_initial_scrollback().await;

    debug!(session_id = %session.id, "starting TUI");
    let result = App {
        session: &session,
        runtime: runtime.clone(),
        events: events_rx,
    }
    .run()
    .await;

    runtime.shutdown();
    result
}
