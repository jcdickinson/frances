use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::ProtocolError;
use crate::feed::{self, DecodeContext, InboundFeeds, OutboundFeeds, Writer};
use crate::frame::Connection;
use crate::frame::RawMessage;

/// Serialized writing half of a multiplexed protocol connection.
#[derive(Clone)]
pub struct ProtocolWriter {
    inner: Writer,
}

impl ProtocolWriter {
    pub async fn send<T>(&self, value: T) -> Result<(), ProtocolError>
    where
        T: Serialize + Send + 'static,
    {
        self.inner.send(value).await
    }
}

/// Reading half of a multiplexed protocol connection. Feed traffic is routed
/// internally; `receive` returns only ordinary top-level messages.
pub struct ProtocolReader {
    messages: tokio::sync::mpsc::Receiver<Result<RawMessage, ProtocolError>>,
    context: DecodeContext,
}

impl ProtocolReader {
    pub async fn receive<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        let Some(raw) = self.messages.recv().await else {
            return Ok(None);
        };
        feed::decode(raw?, self.context.clone()).map(Some)
    }
}

/// Construct independently-driven read/write halves over one full-duplex
/// transport. The writer is serialized by a bounded queue.
pub fn multiplex<R, W>(reader: R, writer: W) -> (ProtocolReader, ProtocolWriter)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let feeds = feed::inbound_feeds();
    let (writer_handle, requests, outbound_feeds) = feed::writer_channel();
    let context = feed::decode_context(feeds.clone(), writer_handle.clone());
    tokio::spawn(feed::run_writer(Connection::new(writer), requests));
    let (messages, message_receiver) = tokio::sync::mpsc::channel(32);
    tokio::spawn(run_reader(
        Connection::new(reader),
        feeds.clone(),
        outbound_feeds,
        messages,
    ));
    (
        ProtocolReader {
            messages: message_receiver,
            context,
        },
        ProtocolWriter {
            inner: writer_handle,
        },
    )
}

async fn run_reader<R: AsyncRead + Unpin>(
    mut connection: Connection<R>,
    feeds: InboundFeeds,
    outbound_feeds: OutboundFeeds,
    messages: tokio::sync::mpsc::Sender<Result<RawMessage, ProtocolError>>,
) {
    loop {
        let raw = match connection.receive_raw().await {
            Ok(Some(raw)) => raw,
            Ok(None) => return,
            Err(error) => {
                let _ = messages.send(Err(error)).await;
                return;
            }
        };
        match feed::route_feed(raw, &feeds, &outbound_feeds).await {
            Ok(Some(raw)) => {
                if messages.send(Ok(raw)).await.is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => {
                let _ = messages.send(Err(error)).await;
                return;
            }
        }
    }
}
