mod install;
mod tty;
mod tui;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use frances_session::context::InvocationContext;
use frances_session::runtime::{SessionRuntime, StartOverrides, install_logging};
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
    New {
        /// Workflow to start in the new session. Defaults to `default_workflow`.
        workflow: Option<String>,
    },
    /// Write a starter config (asking a few questions if config.toml is
    /// absent) and install the `main` workflow into the config dir.
    Install {
        /// Point the config at the in-repo workflow script instead of
        /// copying the embedded one into the config dir.
        #[arg(long)]
        local: bool,
    },
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

    if let Some(Command::Install { local }) = cli.command {
        return install::run(local);
    }

    let tty_key = tty::controlling_tty_key()?;
    let paths = Paths::discover()?;

    let new_workflow = match &cli.command {
        Some(Command::New { workflow }) => workflow.clone(),
        _ => None,
    };

    if matches!(cli.command, Some(Command::New { .. }))
        && paths.resolve_tty_link(&tty_key)?.is_some()
    {
        paths.unlink_tty(&tty_key)?;
    }

    let invocation = InvocationContext::capture(Some(tty_key.clone()));
    let session = paths.resolve_or_create_for_tty(&tty_key, invocation.process.cwd.clone())?;

    install_logging(&session)?;
    let db = store::open(&session).await?;
    let overrides = start_overrides(new_workflow);
    let (runtime, events_rx) =
        SessionRuntime::start_with(session.clone(), db, invocation, overrides).await?;
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

fn start_overrides(workflow: Option<String>) -> StartOverrides {
    StartOverrides {
        default_workflow: workflow,
        ..StartOverrides::default()
    }
}
