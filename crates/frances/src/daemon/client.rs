use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Serialize, de::DeserializeOwned};
use tracing::trace;

use crate::context::InvocationContext;
use crate::daemon::protocol::{
    ClientRequest, ClientResponse, ControlRequest, ControlResponse, DaemonStatus,
};
use crate::session::Session;

pub fn ping(session: &Session) -> Result<()> {
    match send_control(session, ControlRequest::Ping)? {
        ControlResponse::Pong => Ok(()),
        ControlResponse::Error(message) => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected ping response: {other:?}")),
    }
}

pub fn status(session: &Session) -> Result<DaemonStatus> {
    match send_control(session, ControlRequest::Status)? {
        ControlResponse::Status(status) => Ok(status),
        ControlResponse::Error(message) => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected status response: {other:?}")),
    }
}

pub fn stop(session: &Session, delete_state: bool) -> Result<()> {
    match send_control(session, ControlRequest::Stop { delete_state })? {
        ControlResponse::Stopping => Ok(()),
        ControlResponse::Error(message) => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected stop response: {other:?}")),
    }
}

pub fn attach(session: &Session, context: InvocationContext) -> Result<ClientResponse> {
    trace!(session_id = %session.id, "sending attach request");
    send_client(session, ClientRequest::Attach { context })
}

pub fn detach(session: &Session) -> Result<()> {
    match send_client(session, ClientRequest::Detach)? {
        ClientResponse::Detached => Ok(()),
        ClientResponse::Error(message) => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected detach response: {other:?}")),
    }
}

pub fn send_control(session: &Session, request: ControlRequest) -> Result<ControlResponse> {
    let mut stream = connect(&session.control_socket_path())
        .with_context(|| format!("failed to connect control socket for {}", session.id))?;
    write_message(&mut stream, &request)?;
    read_message(&mut stream)
}

pub fn send_client(session: &Session, request: ClientRequest) -> Result<ClientResponse> {
    let mut stream = connect(&session.client_socket_path())
        .with_context(|| format!("failed to connect client socket for {}", session.id))?;
    write_message(&mut stream, &request)?;
    read_message(&mut stream)
}

fn connect(path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(path)
        .with_context(|| format!("failed to connect to {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok(stream)
}

pub fn remove_socket_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing socket {}", path.display()))
        }
    }
}

pub fn write_message<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    let len = u32::try_from(bytes.len()).context("message too large for protocol framing")?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

pub fn read_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut len_bytes = [0_u8; 4];
    stream.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    let (message, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())?;
    Ok(message)
}
