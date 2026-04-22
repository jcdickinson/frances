use std::fs;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::daemon::client::{read_message, remove_socket_if_present, write_message};
use crate::daemon::protocol::{ClientRequest, ClientResponse, ControlRequest, ControlResponse, DaemonStatus};
use crate::session::Session;

#[derive(Debug)]
struct ServerState {
    session: Session,
    client_attached: std::sync::Mutex<bool>,
    shutdown: AtomicBool,
    daemon_pid: u32,
}

pub fn run(session: Session) -> Result<()> {
    fs::create_dir_all(&session.runtime_dir)
        .with_context(|| format!("failed to create runtime dir {}", session.runtime_dir.display()))?;

    remove_socket_if_present(&session.control_socket_path())?;
    remove_socket_if_present(&session.client_socket_path())?;

    let control_listener = bind_listener(&session.control_socket_path())?;
    let client_listener = bind_listener(&session.client_socket_path())?;

    fs::write(session.pid_path(), std::process::id().to_string())
        .with_context(|| format!("failed writing pid file for {}", session.id))?;

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: std::sync::Mutex::new(false),
        shutdown: AtomicBool::new(false),
        daemon_pid: std::process::id(),
    });

    control_listener.set_nonblocking(true)?;
    client_listener.set_nonblocking(true)?;

    while !state.shutdown.load(Ordering::SeqCst) {
        accept_control(&control_listener, &state)?;
        accept_client(&client_listener, &state)?;
        thread::sleep(Duration::from_millis(25));
    }

    let _ = fs::remove_file(session.pid_path());
    let _ = fs::remove_file(session.control_socket_path());
    let _ = fs::remove_file(session.client_socket_path());

    Ok(())
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))
}

fn accept_control(listener: &UnixListener, state: &Arc<ServerState>) -> Result<()> {
    match listener.accept() {
        Ok((stream, _)) => {
            let state = Arc::clone(state);
            thread::spawn(move || {
                if let Err(error) = handle_control(stream, &state) {
                    eprintln!("frances control handler error: {error:#}");
                }
            });
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error).context("control accept failed"),
    }
}

fn accept_client(listener: &UnixListener, state: &Arc<ServerState>) -> Result<()> {
    match listener.accept() {
        Ok((stream, _)) => {
            let state = Arc::clone(state);
            thread::spawn(move || {
                if let Err(error) = handle_client(stream, &state) {
                    eprintln!("frances client handler error: {error:#}");
                }
            });
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error).context("client accept failed"),
    }
}

fn handle_control(mut stream: UnixStream, state: &Arc<ServerState>) -> Result<()> {
    let request: ControlRequest = read_message(&mut stream)?;
    let response = match request {
        ControlRequest::Ping => ControlResponse::Pong,
        ControlRequest::Status => ControlResponse::Status(daemon_status(state)),
        ControlRequest::Stop { .. } => {
            state.shutdown.store(true, Ordering::SeqCst);
            ControlResponse::Stopping
        }
    };
    write_message(&mut stream, &response)
}

fn handle_client(mut stream: UnixStream, state: &Arc<ServerState>) -> Result<()> {
    let request: ClientRequest = read_message(&mut stream)?;
    let response = match request {
        ClientRequest::Attach { .. } => {
            let mut attached = state.client_attached.lock().expect("client_attached poisoned");
            if *attached {
                ClientResponse::Busy
            } else {
                *attached = true;
                ClientResponse::Attached {
                    session_id: state.session.id.clone(),
                }
            }
        }
        ClientRequest::Detach => {
            let mut attached = state.client_attached.lock().expect("client_attached poisoned");
            *attached = false;
            ClientResponse::Detached
        }
    };

    write_message(&mut stream, &response)
}

fn daemon_status(state: &ServerState) -> DaemonStatus {
    DaemonStatus {
        session_id: state.session.id.clone(),
        client_attached: *state.client_attached.lock().expect("client_attached poisoned"),
        daemon_pid: state.daemon_pid,
        control_socket_path: state.session.control_socket_path(),
        client_socket_path: state.session.client_socket_path(),
        protocol_version: 1,
    }
}
