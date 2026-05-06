use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tarpc::client::RpcError;
use tarpc::context;
use tarpc::tokio_serde::formats::Bincode;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::trace;

use crate::context::InvocationContext;
use crate::daemon::protocol::{
    AttachResponse, ClientClient, DaemonPid, DaemonStatus, PromptId, SessionId, StreamFrame,
};
use crate::session::Session;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),
    #[error("encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    #[error("decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),
    #[error("message too large for protocol framing")]
    MessageTooLarge,
    /// Control socket reached EOF before sending the banner line.
    #[error("daemon closed before sending control banner")]
    NoBanner,
    /// Banner line was present but didn't parse as a hex u64.
    #[error("malformed control banner: {0:?}")]
    MalformedBanner(String),
    /// Control response was empty / had no status line.
    #[error("empty control response")]
    EmptyControlResponse,
    /// Control response status line wasn't `ok` or `err <msg>`.
    #[error("malformed control status line: {0:?}")]
    MalformedControlStatus(String),
    /// The daemon explicitly returned `err <msg>` over a socket. The string
    /// is the daemon's message verbatim.
    #[error("daemon: {0}")]
    Server(String),
}

async fn connect_client(session: &Session) -> Result<ClientClient, ClientError> {
    let path = session.client_socket_path();
    trace!(session_id = %session.id, path = %path.display(), "connecting client");
    let transport = tarpc::serde_transport::unix::connect(&path, Bincode::default).await?;
    Ok(ClientClient::new(tarpc::client::Config::default(), transport).spawn())
}

// The control socket uses a deliberately tiny newline-delimited TEXT protocol
// (see `serve_control` in server.rs for the full rationale). Each connection
// starts with the daemon writing its `PROTOCOL_VERSION` as a hex banner line,
// then the client sends one command and reads `ok\n` or `err <msg>\n` followed
// by optional `key=value\n` lines and a blank line terminator.
//
// `daemon_version` reads only the banner — used by `ensure_daemon` for a cheap
// version-mismatch check without sending any command.

pub async fn daemon_version(session: &Session) -> Result<u64, ClientError> {
    let stream = UnixStream::connect(session.control_socket_path()).await?;
    let mut reader = BufReader::new(stream);
    read_banner(&mut reader).await
}

pub async fn ping(session: &Session) -> Result<(), ClientError> {
    request_control(session, "ping").await?;
    Ok(())
}

pub async fn status(session: &Session) -> Result<DaemonStatus, ClientError> {
    let (banner_version, kvs) = request_control(session, "status").await?;
    let mut session_id = String::new();
    let mut client_attached = false;
    let mut daemon_pid: u32 = 0;
    let mut protocol_version: u64 = banner_version;
    for line in kvs {
        let mut parts = line.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        match key {
            "session_id" => session_id = val.to_string(),
            "client_attached" => client_attached = val == "true",
            "daemon_pid" => daemon_pid = val.parse().unwrap_or(0),
            "protocol_version" => {
                protocol_version = u64::from_str_radix(val, 16).unwrap_or(banner_version);
            }
            _ => {}
        }
    }
    Ok(DaemonStatus {
        session_id: SessionId(session_id),
        client_attached,
        daemon_pid: DaemonPid(daemon_pid),
        control_socket_path: session.control_socket_path(),
        client_socket_path: session.client_socket_path(),
        events_socket_path: session.events_socket_path(),
        protocol_version,
    })
}

pub async fn stop(session: &Session, delete_state: bool) -> Result<(), ClientError> {
    let cmd = if delete_state {
        "stop delete=1"
    } else {
        "stop"
    };
    request_control(session, cmd).await?;
    Ok(())
}

async fn read_banner(reader: &mut BufReader<UnixStream>) -> Result<u64, ClientError> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(ClientError::NoBanner);
    }
    let trimmed = line.trim();
    u64::from_str_radix(trimmed, 16).map_err(|_| ClientError::MalformedBanner(trimmed.to_string()))
}

async fn request_control(
    session: &Session,
    request: &str,
) -> Result<(u64, Vec<String>), ClientError> {
    let stream = UnixStream::connect(session.control_socket_path()).await?;
    let mut reader = BufReader::new(stream);
    let banner_version = read_banner(&mut reader).await?;

    let stream = reader.get_mut();
    stream.write_all(format!("{request}\n").as_bytes()).await?;
    stream.flush().await?;

    let mut lines: Vec<String> = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed);
    }

    if lines.is_empty() {
        return Err(ClientError::EmptyControlResponse);
    }
    let head = lines.remove(0);
    if let Some(msg) = head.strip_prefix("err ") {
        return Err(ClientError::Server(msg.to_string()));
    }
    if head != "ok" {
        return Err(ClientError::MalformedControlStatus(head));
    }
    Ok((banner_version, lines))
}

pub async fn attach(
    session: &Session,
    invocation: InvocationContext,
) -> Result<AttachResponse, ClientError> {
    trace!(session_id = %session.id, "sending attach request");
    let client = connect_client(session).await?;
    Ok(client.attach(context::current(), invocation).await?)
}

pub async fn detach(session: &Session) -> Result<(), ClientError> {
    let client = connect_client(session).await?;
    client.detach(context::current()).await?;
    Ok(())
}

pub async fn prompt_stream<F>(
    session: &Session,
    prompt_id: PromptId,
    text: String,
    mut on_frame: F,
) -> Result<(), ClientError>
where
    F: FnMut(StreamFrame),
{
    trace!(session_id = %session.id, prompt_id = %prompt_id, "opening events socket");
    let mut events = UnixStream::connect(session.events_socket_path()).await?;
    write_message(&mut events, &prompt_id).await?;

    let client = connect_client(session).await?;
    client
        .prompt(context::current(), prompt_id, text)
        .await?
        .map_err(ClientError::Server)?;

    loop {
        let frame: StreamFrame = read_message(&mut events).await?;
        let stop = matches!(frame, StreamFrame::Done | StreamFrame::Error(_));
        on_frame(frame);
        if stop {
            return Ok(());
        }
    }
}

pub async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), ClientError> {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    let len = u32::try_from(bytes.len()).map_err(|_| ClientError::MessageTooLarge)?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, ClientError> {
    let mut len_bytes = [0_u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await?;
    let (message, _) = bincode::serde::decode_from_slice(&payload, bincode::config::standard())?;
    Ok(message)
}

pub fn remove_socket_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
