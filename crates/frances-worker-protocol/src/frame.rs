use std::collections::HashMap;
use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::Content;
use crate::content::{self, Encoded, PendingContent};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UNCLAIMED_ATTACHMENTS: usize = 16;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("encode JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("decode JSON: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

pub struct Connection<S> {
    stream: S,
    received: HashMap<u64, Content>,
}

impl<S> Connection<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            received: HashMap::new(),
        }
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<S: AsyncWrite + Unpin> Connection<S> {
    pub async fn send<T: Serialize>(&mut self, value: &T) -> Result<(), ProtocolError> {
        let encoded = content::encode(value).map_err(ProtocolError::Encode)?;
        self.send_encoded(encoded).await
    }

    pub(crate) async fn send_encoded(&mut self, encoded: Encoded) -> Result<(), ProtocolError> {
        let (json, pending) = encoded;

        for (id, source) in pending {
            self.send_content(id, source).await?;
        }
        self.write_header("application/json", None, json.len() as u64)
            .await?;
        self.stream.write_all(&json).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn send_content(&mut self, id: u64, source: PendingContent) -> Result<(), ProtocolError> {
        let file = tempfile::tempfile()?;
        let mut staged = tokio::fs::File::from_std(file);
        let mut reader = match source {
            PendingContent::Reader(reader) => reader,
            PendingContent::File(path) => Box::pin(tokio::fs::File::open(path).await?),
        };
        let length = tokio::io::copy(&mut reader, &mut staged).await?;
        if length > MAX_ATTACHMENT_BYTES {
            return Err(ProtocolError::InvalidFrame(
                "outgoing attachment is too large".into(),
            ));
        }
        staged.rewind().await?;
        self.write_header("application/octet-stream", Some(id), length)
            .await?;
        tokio::io::copy(&mut staged, &mut self.stream).await?;
        Ok(())
    }

    async fn write_header(
        &mut self,
        content_type: &str,
        content_id: Option<u64>,
        content_length: u64,
    ) -> io::Result<()> {
        let mut header =
            format!("Content-Length: {content_length}\r\nContent-Type: {content_type}\r\n");
        if let Some(id) = content_id {
            header.push_str(&format!("Content-Id: {id}\r\n"));
        }
        header.push_str("\r\n");
        self.stream.write_all(header.as_bytes()).await
    }
}

impl<S: AsyncRead + Unpin> Connection<S> {
    pub async fn receive<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        let Some(raw) = self.receive_raw().await? else {
            return Ok(None);
        };
        raw.decode().map(Some)
    }

    pub(crate) async fn receive_raw(&mut self) -> Result<Option<RawMessage>, ProtocolError> {
        loop {
            let Some(header) = self.read_header().await? else {
                return Ok(None);
            };
            match header.content_type.as_str() {
                "application/octet-stream" => self.receive_content(header).await?,
                "application/json" => {
                    if header.content_id.is_some() {
                        return Err(ProtocolError::InvalidFrame(
                            "JSON frame has Content-Id".into(),
                        ));
                    }
                    if header.content_length > MAX_JSON_BYTES {
                        return Err(ProtocolError::InvalidFrame(
                            "JSON frame is too large".into(),
                        ));
                    }
                    let mut json = vec![0; header.content_length as usize];
                    self.stream.read_exact(&mut json).await?;
                    return Ok(Some(RawMessage {
                        json,
                        attachments: std::mem::take(&mut self.received),
                    }));
                }
                other => {
                    return Err(ProtocolError::InvalidFrame(format!(
                        "unsupported Content-Type {other:?}"
                    )));
                }
            }
        }
    }

    async fn receive_content(&mut self, header: Header) -> Result<(), ProtocolError> {
        let id = header.content_id.ok_or_else(|| {
            ProtocolError::InvalidFrame("attachment frame has no Content-Id".into())
        })?;
        if header.content_length > MAX_ATTACHMENT_BYTES {
            return Err(ProtocolError::InvalidFrame(
                "incoming attachment is too large".into(),
            ));
        }
        if self.received.len() >= MAX_UNCLAIMED_ATTACHMENTS {
            return Err(ProtocolError::InvalidFrame(
                "too many unclaimed attachments".into(),
            ));
        }
        if self.received.contains_key(&id) {
            return Err(ProtocolError::InvalidFrame(format!(
                "duplicate attachment {id}"
            )));
        }

        let tempfile = NamedTempFile::new()?;
        let (file, path) = tempfile.into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let mut limited = (&mut self.stream).take(header.content_length);
        let copied = tokio::io::copy(&mut limited, &mut file).await?;
        if copied != header.content_length {
            return Err(ProtocolError::InvalidFrame(format!(
                "attachment {id} ended after {copied} of {} bytes",
                header.content_length
            )));
        }
        file.flush().await?;
        drop(file);
        self.received.insert(id, content::staged(path));
        Ok(())
    }

    async fn read_header(&mut self) -> Result<Option<Header>, ProtocolError> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.stream.read_u8().await;
            match byte {
                Ok(byte) => bytes.push(byte),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && bytes.is_empty() => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
            if bytes.len() > MAX_HEADER_BYTES {
                return Err(ProtocolError::InvalidFrame("header is too large".into()));
            }
        }

        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ProtocolError::InvalidFrame("header is not ASCII".into()))?;
        let mut content_length = None;
        let mut content_type = None;
        let mut content_id = None;
        for line in text.trim_end().split("\r\n") {
            let (name, value) = line.split_once(':').ok_or_else(|| {
                ProtocolError::InvalidFrame(format!("malformed header line {line:?}"))
            })?;
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => {
                    content_length = Some(value.parse().map_err(|_| {
                        ProtocolError::InvalidFrame("invalid Content-Length".into())
                    })?)
                }
                "content-type" => content_type = Some(value.to_owned()),
                "content-id" => {
                    content_id =
                        Some(value.parse().map_err(|_| {
                            ProtocolError::InvalidFrame("invalid Content-Id".into())
                        })?)
                }
                other => {
                    return Err(ProtocolError::InvalidFrame(format!(
                        "unknown header {other:?}"
                    )));
                }
            }
        }
        Ok(Some(Header {
            content_length: content_length
                .ok_or_else(|| ProtocolError::InvalidFrame("missing Content-Length".into()))?,
            content_type: content_type
                .ok_or_else(|| ProtocolError::InvalidFrame("missing Content-Type".into()))?,
            content_id,
        }))
    }
}

pub struct RawMessage {
    pub json: Vec<u8>,
    attachments: HashMap<u64, Content>,
}

impl RawMessage {
    pub fn decode<T: DeserializeOwned>(self) -> Result<T, ProtocolError> {
        content::decode(&self.json, self.attachments).map_err(ProtocolError::Decode)
    }
}

struct Header {
    content_length: u64,
    content_type: String,
    content_id: Option<u64>,
}
