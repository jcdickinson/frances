mod daemon;
mod session;
mod tty;

use anyhow::{Result, anyhow};
use clap::Parser;
use daemon::{client, protocol, server, spawn};
use session::{Paths, Session};

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

fn main() {
    if let Err(error) = real_main() {
        eprintln!("frances: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(session_id) = cli.daemon {
        let paths = Paths::discover()?;
        let session = paths.load_session(&session_id)?;
        return server::run(session);
    }

    let tty_key = tty::controlling_tty_key()?;
    let paths = Paths::discover()?;

    if cli.status {
        let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
        let status = client::status(&session)?;
        println!("session_id: {}", status.session_id);
        println!("client_attached: {}", status.client_attached);
        println!("daemon_pid: {}", status.daemon_pid);
        println!("control_socket: {}", status.control_socket_path.display());
        println!("client_socket: {}", status.client_socket_path.display());
        println!("protocol_version: {}", status.protocol_version);
        return Ok(());
    }

    if cli.stop {
        let session = resolve_existing_session_for_tty(&paths, &tty_key)?;
        client::stop(&session, false)?;
        println!("frances session stopping: {}", session.id);
        return Ok(());
    }

    let cwd = std::env::current_dir().ok();
    let session = paths.resolve_or_create_for_tty(&tty_key, cwd)?;

    spawn::ensure_daemon(&session)?;

    match client::attach(&session, Some(tty_key), std::env::current_dir().ok())? {
        protocol::ClientResponse::Attached { session_id } => {
            let _ = client::detach(&session);
            println!("frances session ready: {session_id}");
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
    paths.resolve_tty_link(tty_key)?
        .ok_or_else(|| anyhow!("no frances session is linked to the current TTY"))
}
