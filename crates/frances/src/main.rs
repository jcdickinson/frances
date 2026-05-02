mod context;
mod daemon;
mod history;
mod llm;
mod session;
mod store;
mod tty;
mod ui;

use anyhow::{Result, anyhow};
use clap::Parser;
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

    #[arg(long, conflicts_with = "stop")]
    status: bool,

    #[arg(long, conflicts_with = "status")]
    stop: bool,
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
        let _store = store::Store::open(&paths).await?;
        return server::run(session);
    }

    let tty_key = tty::controlling_tty_key()?;
    let paths = Paths::discover()?;

    if cli.status {
        let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
        let status = client::status(&session)?;
        App { status: &status }.run()?;
        return Ok(());
    }

    if cli.stop {
        let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
        client::stop(&session, false)?;
        println!("frances session stopping: {}", session.id);
        return Ok(());
    }

    let invocation = InvocationContext::capture(Some(tty_key.clone()));
    let session = paths.resolve_or_create_for_tty(&tty_key, invocation.process.cwd.clone())?;

    spawn::ensure_daemon(&session)?;

    debug!(session_id = %session.id, "attaching client to daemon");
    match client::attach(&session, invocation)? {
        protocol::ClientResponse::Attached { session_id: _ } => {
            let status = client::status(&session)?;
            let _ = client::detach(&session);
            App { status: &status }.run()?;
        }
        protocol::ClientResponse::Busy => {
            println!("frances session busy: {}", session.id);
        }
        protocol::ClientResponse::Detached => {
            println!("frances session detached: {}", session.id);
        }
        protocol::ClientResponse::Error(message) => {
            return Err(anyhow!(message));
        }
    }

    Ok(())
}

fn resolve_existing_session_for_tty(paths: &Paths, tty_key: &str) -> Result<Session> {
    paths
        .resolve_tty_link(tty_key)?
        .ok_or_else(|| anyhow!("no frances session is linked to the current TTY"))
}
