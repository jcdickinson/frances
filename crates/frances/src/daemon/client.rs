use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tarpc::client::RpcError;
use tarpc::context;
use tarpc::tokio_serde::formats::Bincode;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::trace;

use crate::context::InvocationContext;
use crate::daemon::protocol::{
    AttachResponse, ClientClient, ControlClient, DaemonStatus, PromptId, StreamFrame,
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
    #[error("daemon: {0}")]
    Server(String),
}

async fn connect_control(session: &Session) -> Result<ControlClient, ClientError> {
    let path = session.control_socket_path();
    trace!(session_id = %session.id, path = %path.display(), "connecting control");
    let transport = tarpc::serde_transport::unix::connect(&path, Bincode::default).await?;
    Ok(ControlClient::new(tarpc::client::Config::default(), transport).spawn())
}

async fn connect_client(session: &Session) -> Result<ClientClient, ClientError> {
    let path = session.client_socket_path();
    trace!(session_id = %session.id, path = %path.display(), "connecting client");
    let transport = tarpc::serde_transport::unix::connect(&path, Bincode::default).await?;
    Ok(ClientClient::new(tarpc::client::Config::default(), transport).spawn())
}

pub async fn ping(session: &Session) -> Result<(), ClientError> {
    let client = connect_control(session).await?;
    client.ping(context::current()).await?;
    Ok(())
}

pub async fn status(session: &Session) -> Result<DaemonStatus, ClientError> {
    let client = connect_control(session).await?;
    Ok(client.status(context::current()).await?)
}

pub async fn stop(session: &Session, delete_state: bool) -> Result<(), ClientError> {
    let client = connect_control(session).await?;
    client.stop(context::current(), delete_state).await?;
    Ok(())
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
    trace!(session_id = %session.id, prompt_id, "opening events socket");
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
