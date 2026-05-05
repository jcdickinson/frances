mod context;
mod daemon;
mod history;
mod llm;
mod session;
mod store;
mod tty;
mod ui;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use daemon::{client, protocol, server, spawn};
use tracing::debug;

use crate::context::InvocationContext;
use crate::session::{Paths, Session};
use crate::ui::App;

#[derive(Debug, Parser)]
#[command(name = "frances")]
struct Cli {
    #[arg(long, hide = true)]
    daemon: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the daemon for the current TTY.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Stop the daemon for the current TTY and unlink its session, so the
    /// next invocation starts a fresh session.
    New,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Show daemon status for the current TTY.
    Status,
    /// Stop the daemon for the current TTY.
    Stop,
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

    if let Some(session_id) = cli.daemon {
        let paths = Paths::discover()?;
        let session = paths.load_session(&session_id)?;
        server::install_logging(&session)?;
        let db = store::Database::open(&session).await?;
        return server::run(session, db).await;
    }

    let tty_key = tty::controlling_tty_key()?;
    let paths = Paths::discover()?;

    match cli.command {
        Some(Command::Daemon {
            action: DaemonAction::Status,
        }) => {
            let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
            let status = client::status(&session).await?;
            App {
                session: &session,
                status: &status,
            }
            .run()?;
            return Ok(());
        }
        Some(Command::Daemon {
            action: DaemonAction::Stop,
        }) => {
            let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
            client::stop(&session, false).await?;
            println!("frances session stopping: {}", session.id);
            return Ok(());
        }
        Some(Command::New) => {
            if let Some(session) = paths.resolve_tty_link(&tty_key)? {
                if let Err(error) = client::stop(&session, false).await {
                    debug!(%error, "stop request failed during `new`; continuing to unlink");
                }
                paths.unlink_tty(&tty_key)?;
            }
        }
        None => {}
    }

    let invocation = InvocationContext::capture(Some(tty_key.clone()));
    let session = paths.resolve_or_create_for_tty(&tty_key, invocation.process.cwd.clone())?;

    spawn::ensure_daemon(&session).await?;

    debug!(session_id = %session.id, "attaching client to daemon");
    match client::attach(&session, invocation).await? {
        protocol::AttachResponse::Attached { session_id: _ } => {
            let status = client::status(&session).await?;
            let _ = client::detach(&session).await;
            App {
                session: &session,
                status: &status,
            }
            .run()?;
        }
        protocol::AttachResponse::Busy => {
            println!("frances session busy: {}", session.id);
        }
    }

    Ok(())
}

fn resolve_existing_session_for_tty(paths: &Paths, tty_key: &str) -> Result<Session> {
    paths
        .resolve_tty_link(tty_key)?
        .ok_or_else(|| anyhow!("no frances session is linked to the current TTY"))
}
