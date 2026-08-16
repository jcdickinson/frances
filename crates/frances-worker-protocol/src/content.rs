use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::TempPath;
use tokio::io::{AsyncRead, ReadBuf};

pub(crate) type BoxReader = Pin<Box<dyn AsyncRead + Send + 'static>>;

pub(crate) enum PendingContent {
    Reader(BoxReader),
    File(TempPath),
}

pub(crate) type Encoded = (Vec<u8>, Vec<(u64, PendingContent)>);

enum ContentState {
    Pending(PendingContent),
}

/// A single-use finite byte stream.
///
/// Attachment identifiers, transport framing, and any tempfile backing are
/// deliberately hidden. Dropping a received `Content` without reading it also
/// removes its staged data.
pub struct Content {
    state: Mutex<Option<ContentState>>,
}

impl Content {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::from_async_read(std::io::Cursor::new(bytes))
    }

    pub fn from_async_read(reader: impl AsyncRead + Send + 'static) -> Self {
        Self {
            state: Mutex::new(Some(ContentState::Pending(PendingContent::Reader(
                Box::pin(reader),
            )))),
        }
    }

    pub async fn into_async_read(self) -> io::Result<ContentReader> {
        let state = self.take()?;
        match state {
            PendingContent::Reader(reader) => Ok(ContentReader {
                reader,
                _staged_path: None,
            }),
            PendingContent::File(path) => {
                let file = tokio::fs::File::open(&path).await?;
                Ok(ContentReader {
                    reader: Box::pin(file),
                    _staged_path: Some(path),
                })
            }
        }
    }

    pub async fn copy_to(
        self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> io::Result<u64> {
        let mut reader = self.into_async_read().await?;
        tokio::io::copy(&mut reader, writer).await
    }

    fn from_staged_file(path: TempPath) -> Self {
        Self {
            state: Mutex::new(Some(ContentState::Pending(PendingContent::File(path)))),
        }
    }

    fn take(&self) -> io::Result<PendingContent> {
        let state = self
            .state
            .lock()
            .expect("content state mutex poisoned")
            .take()
            .ok_or_else(|| io::Error::other("content has already been consumed"))?;
        let ContentState::Pending(content) = state;
        Ok(content)
    }
}

pub struct ContentReader {
    reader: BoxReader,
    // Kept alive until after the reader is dropped.
    _staged_path: Option<TempPath>,
}

impl AsyncRead for ContentReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.reader.as_mut().poll_read(cx, buf)
    }
}

impl std::fmt::Debug for Content {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Content").finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct AttachmentRef {
    attachment: u64,
}

struct EncodeContext {
    next_id: u64,
    pending: Vec<(u64, PendingContent)>,
}

thread_local! {
    static ENCODE_CONTEXT: RefCell<Option<EncodeContext>> = const { RefCell::new(None) };
    static DECODE_CONTEXT: RefCell<Option<HashMap<u64, Content>>> = const { RefCell::new(None) };
}

impl Serialize for Content {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ENCODE_CONTEXT.with_borrow_mut(|slot| {
            let context = slot
                .as_mut()
                .ok_or_else(|| serde::ser::Error::custom("Content serialized outside protocol"))?;
            let content = self.take().map_err(serde::ser::Error::custom)?;
            let id = context.next_id;
            context.next_id += 1;
            context.pending.push((id, content));
            AttachmentRef { attachment: id }.serialize(serializer)
        })
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let reference = AttachmentRef::deserialize(deserializer)?;
        DECODE_CONTEXT.with_borrow_mut(|slot| {
            slot.as_mut()
                .ok_or_else(|| D::Error::custom("Content deserialized outside protocol"))?
                .remove(&reference.attachment)
                .ok_or_else(|| {
                    D::Error::custom(format!(
                        "unknown or already claimed attachment {}",
                        reference.attachment
                    ))
                })
        })
    }
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Encoded, serde_json::Error> {
    ENCODE_CONTEXT.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "nested protocol encoding");
        *slot = Some(EncodeContext {
            next_id: 1,
            pending: Vec::new(),
        });
    });

    let encoded = serde_json::to_vec(value);
    let pending = ENCODE_CONTEXT.with_borrow_mut(|slot| {
        slot.take()
            .expect("protocol encoding context disappeared")
            .pending
    });
    encoded.map(|json| (json, pending))
}

pub(crate) fn decode<'de, T: Deserialize<'de>>(
    json: &'de [u8],
    attachments: HashMap<u64, Content>,
) -> Result<T, serde_json::Error> {
    DECODE_CONTEXT.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "nested protocol decoding");
        *slot = Some(attachments);
    });

    let decoded = serde_json::from_slice(json);
    let unclaimed = DECODE_CONTEXT.with_borrow_mut(|slot| {
        slot.take()
            .expect("protocol decoding context disappeared")
            .len()
    });

    match decoded {
        Ok(_) if unclaimed != 0 => Err(serde::de::Error::custom(format!(
            "message left {unclaimed} attachment(s) unclaimed"
        ))),
        other => other,
    }
}

pub(crate) fn staged(path: TempPath) -> Content {
    Content::from_staged_file(path)
}
