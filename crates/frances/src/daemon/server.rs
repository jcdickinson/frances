use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, error, info, trace};

use crate::context::InvocationContext;
use crate::daemon::client::{read_message, remove_socket_if_present, write_message};
use crate::daemon::protocol::{
    ClientRequest, ClientResponse, ControlRequest, ControlResponse, DaemonStatus,
};
use crate::session::Session;

#[derive(Debug)]
struct ServerState {
    session: Session,
    client_attached: std::sync::Mutex<bool>,
    last_context: std::sync::Mutex<Option<InvocationContext>>,
    shutdown: AtomicBool,
    daemon_pid: u32,
}

pub fn install_logging(session: &Session) -> Result<()> {
    let log_path = session.dir.join("daemon.log");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;

    let fd = file.as_raw_fd();
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) < 0 {
            return Err(io::Error::last_os_error()).context("dup2 stdout to daemon log");
        }
        if libc::dup2(fd, libc::STDERR_FILENO) < 0 {
            return Err(io::Error::last_os_error()).context("dup2 stderr to daemon log");
        }
    }
    drop(file);

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))?;

    info!(session_id = %session.id, log = %log_path.display(), "daemon logging installed");
    Ok(())
}

pub fn run(session: Session) -> Result<()> {
    debug!(session_id = %session.id, "starting daemon server");

    fs::create_dir_all(&session.runtime_dir).with_context(|| {
        format!(
            "failed to create runtime dir {}",
            session.runtime_dir.display()
        )
    })?;

    remove_socket_if_present(&session.control_socket_path())?;
    remove_socket_if_present(&session.client_socket_path())?;

    let control_listener = bind_listener(&session.control_socket_path())?;
    let client_listener = bind_listener(&session.client_socket_path())?;

    fs::write(session.pid_path(), std::process::id().to_string())
        .with_context(|| format!("failed writing pid file for {}", session.id))?;

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: std::sync::Mutex::new(false),
        last_context: std::sync::Mutex::new(None),
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
                    error!(error = %error, "frances control handler error");
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
                    error!(error = %error, "frances client handler error");
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
        ClientRequest::Attach { context } => {
            trace!(
                session_id = %state.session.id,
                env_vars = context.process.env.len(),
                has_cwd = context.process.cwd.is_some(),
                "received attach context"
            );

            let mut attached = state
                .client_attached
                .lock()
                .expect("client_attached poisoned");
            if *attached {
                ClientResponse::Busy
            } else {
                *state.last_context.lock().expect("last_context poisoned") = Some(context);
                *attached = true;
                ClientResponse::Attached {
                    session_id: state.session.id.clone(),
                }
            }
        }
        ClientRequest::Detach => {
            let mut attached = state
                .client_attached
                .lock()
                .expect("client_attached poisoned");
            *attached = false;
            ClientResponse::Detached
        }
    };

    write_message(&mut stream, &response)
}

fn daemon_status(state: &ServerState) -> DaemonStatus {
    DaemonStatus {
        session_id: state.session.id.clone(),
        client_attached: *state
            .client_attached
            .lock()
            .expect("client_attached poisoned"),
        daemon_pid: state.daemon_pid,
        control_socket_path: state.session.control_socket_path(),
        client_socket_path: state.session.client_socket_path(),
        protocol_version: 1,
    }
}
