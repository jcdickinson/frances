use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    #[error("decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),
    #[error("message too large for protocol framing")]
    MessageTooLarge,
}

pub async fn write_message<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), TransportError> {
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    let len = u32::try_from(bytes.len()).map_err(|_| TransportError::MessageTooLarge)?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_message<T: DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, TransportError> {
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
