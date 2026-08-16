use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot};

use crate::content::{self, Encoded};
use crate::frame::{Connection, ProtocolError, RawMessage};

const FEED_CAPACITY: usize = 16;

pub type FeedId = u64;
pub(crate) type InboundFeeds = Arc<Mutex<HashMap<FeedId, mpsc::Sender<RawMessage>>>>;
pub(crate) type OutboundFeeds = Arc<Mutex<HashMap<FeedId, tokio::task::AbortHandle>>>;

/// The receiving half of a bounded, ordered protocol feed.
pub struct Feed<T> {
    state: Mutex<Option<FeedState>>,
    marker: PhantomData<fn() -> T>,
}

enum FeedState {
    Local(Box<dyn LocalReceiver + Send>),
    Remote {
        id: FeedId,
        receiver: mpsc::Receiver<RawMessage>,
        context: DecodeContext,
    },
}

trait LocalReceiver {
    fn activate(self: Box<Self>, id: FeedId, writer: Writer) -> tokio::task::AbortHandle;
}

struct TypedReceiver<T> {
    receiver: mpsc::Receiver<T>,
}

impl<T> LocalReceiver for TypedReceiver<T>
where
    T: Serialize + Send + 'static,
{
    fn activate(mut self: Box<Self>, id: FeedId, writer: Writer) -> tokio::task::AbortHandle {
        tokio::spawn(async move {
            while let Some(item) = self.receiver.recv().await {
                if writer.send(FeedItem { feed: id, item }).await.is_err() {
                    return;
                }
            }
            let _ = writer
                .send(FeedEnd {
                    feed: id,
                    end: true,
                })
                .await;
        })
        .abort_handle()
    }
}

pub struct FeedSender<T> {
    sender: mpsc::Sender<T>,
}

impl<T> Clone for FeedSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<T> FeedSender<T> {
    pub async fn send(&self, item: T) -> Result<(), FeedSendError<T>> {
        self.sender
            .send(item)
            .await
            .map_err(|error| FeedSendError(error.0))
    }
}

pub struct FeedSendError<T>(pub T);

impl<T> std::fmt::Debug for FeedSendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FeedSendError(..)")
    }
}

impl<T> std::fmt::Display for FeedSendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("feed receiver is closed")
    }
}

impl<T> std::error::Error for FeedSendError<T> {}

impl<T> Feed<T> {
    pub fn channel() -> (FeedSender<T>, Self)
    where
        T: Serialize + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
        (
            FeedSender { sender },
            Self {
                state: Mutex::new(Some(FeedState::Local(Box::new(TypedReceiver { receiver })))),
                marker: PhantomData,
            },
        )
    }

    pub async fn next(&mut self) -> Result<Option<T>, ProtocolError>
    where
        T: DeserializeOwned,
    {
        let (raw, context) = {
            let state = self
                .state
                .get_mut()
                .expect("feed state mutex poisoned")
                .as_mut()
                .ok_or_else(|| ProtocolError::InvalidFrame("feed has been transferred".into()))?;
            let FeedState::Remote {
                receiver, context, ..
            } = state
            else {
                return Err(ProtocolError::InvalidFrame(
                    "cannot receive from a feed before it is transferred".into(),
                ));
            };
            (receiver.recv().await, context.clone())
        };
        let Some(raw) = raw else {
            return Ok(None);
        };
        let header: FeedHeader =
            serde_json::from_slice(&raw.json).map_err(ProtocolError::Decode)?;
        if header.end.is_some() {
            return Ok(None);
        }
        with_decode_context(context, || {
            raw.decode::<FeedItem<T>>().map(|frame| Some(frame.item))
        })
    }

    /// Return the next already-buffered item without waiting for the wire.
    pub fn try_next(&mut self) -> Result<Option<T>, ProtocolError>
    where
        T: DeserializeOwned,
    {
        let (raw, context) = {
            let state = self
                .state
                .get_mut()
                .expect("feed state mutex poisoned")
                .as_mut()
                .ok_or_else(|| ProtocolError::InvalidFrame("feed has been transferred".into()))?;
            let FeedState::Remote {
                receiver, context, ..
            } = state
            else {
                return Err(ProtocolError::InvalidFrame(
                    "cannot receive from a feed before it is transferred".into(),
                ));
            };
            let raw = match receiver.try_recv() {
                Ok(raw) => raw,
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    return Ok(None);
                }
            };
            (raw, context.clone())
        };
        let header: FeedHeader =
            serde_json::from_slice(&raw.json).map_err(ProtocolError::Decode)?;
        if header.end.is_some() {
            return Ok(None);
        }
        with_decode_context(context, || {
            raw.decode::<FeedItem<T>>().map(|frame| Some(frame.item))
        })
    }
}

impl<T> Serialize for Feed<T>
where
    T: Serialize + Send + 'static,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let receiver = match self.state.lock().expect("feed state mutex poisoned").take() {
            Some(FeedState::Local(receiver)) => receiver,
            _ => {
                return Err(serde::ser::Error::custom(
                    "feed has already been transferred",
                ));
            }
        };
        ENCODE_CONTEXT.with_borrow_mut(|slot| {
            let context = slot
                .as_mut()
                .ok_or_else(|| serde::ser::Error::custom("Feed serialized outside protocol"))?;
            let id = context.ids.fetch_add(1, Ordering::Relaxed);
            context.pending.push(PendingFeed { id, receiver });
            FeedRef { feed: id }.serialize(serializer)
        })
    }
}

impl<'de, T> Deserialize<'de> for Feed<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let reference = FeedRef::deserialize(deserializer)?;
        DECODE_CONTEXT.with_borrow(|slot| {
            let context = slot
                .as_ref()
                .ok_or_else(|| serde::de::Error::custom("Feed deserialized outside protocol"))?;
            let (sender, receiver) = mpsc::channel(FEED_CAPACITY);
            let previous = context
                .feeds
                .lock()
                .expect("inbound feed registry poisoned")
                .insert(reference.feed, sender);
            if previous.is_some() {
                return Err(serde::de::Error::custom("duplicate feed id"));
            }
            Ok(Self {
                state: Mutex::new(Some(FeedState::Remote {
                    id: reference.feed,
                    receiver,
                    context: context.clone(),
                })),
                marker: PhantomData,
            })
        })
    }
}

impl<T> Drop for Feed<T> {
    fn drop(&mut self) {
        let Ok(state) = self.state.get_mut() else {
            return;
        };
        let Some(FeedState::Remote { id, context, .. }) = state.as_ref() else {
            return;
        };
        context
            .feeds
            .lock()
            .expect("inbound feed registry poisoned")
            .remove(id);
        context.writer.try_send(FeedCancel {
            feed: *id,
            cancel: true,
        });
    }
}

impl<T> std::fmt::Debug for Feed<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Feed").finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct FeedRef {
    feed: FeedId,
}

#[derive(Serialize, Deserialize)]
struct FeedItem<T> {
    feed: FeedId,
    item: T,
}

#[derive(Serialize)]
struct FeedEnd {
    feed: FeedId,
    end: bool,
}

#[derive(Serialize)]
struct FeedCancel {
    feed: FeedId,
    cancel: bool,
}

#[derive(Deserialize)]
struct FeedHeader {
    #[allow(dead_code)]
    feed: FeedId,
    end: Option<bool>,
}

pub(crate) struct PendingFeed {
    id: FeedId,
    receiver: Box<dyn LocalReceiver + Send>,
}

struct EncodeContext {
    ids: Arc<AtomicU64>,
    pending: Vec<PendingFeed>,
}

#[derive(Clone)]
pub(crate) struct DecodeContext {
    feeds: InboundFeeds,
    writer: Writer,
}

thread_local! {
    static ENCODE_CONTEXT: RefCell<Option<EncodeContext>> = const { RefCell::new(None) };
    static DECODE_CONTEXT: RefCell<Option<DecodeContext>> = const { RefCell::new(None) };
}

pub(crate) fn encode<T: Serialize>(
    value: &T,
    ids: Arc<AtomicU64>,
) -> Result<(Encoded, Vec<PendingFeed>), ProtocolError> {
    ENCODE_CONTEXT.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "nested feed encoding");
        *slot = Some(EncodeContext {
            ids,
            pending: Vec::new(),
        });
    });
    let encoded = content::encode(value).map_err(ProtocolError::Encode);
    let pending = ENCODE_CONTEXT.with_borrow_mut(|slot| {
        slot.take()
            .expect("feed encoding context disappeared")
            .pending
    });
    encoded.map(|encoded| (encoded, pending))
}

pub(crate) fn decode<T: DeserializeOwned>(
    raw: RawMessage,
    context: DecodeContext,
) -> Result<T, ProtocolError> {
    with_decode_context(context, || raw.decode())
}

fn with_decode_context<T>(
    context: DecodeContext,
    decode: impl FnOnce() -> Result<T, ProtocolError>,
) -> Result<T, ProtocolError> {
    DECODE_CONTEXT.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "nested feed decoding");
        *slot = Some(context);
    });
    let result = decode();
    DECODE_CONTEXT.with_borrow_mut(|slot| {
        slot.take();
    });
    result
}

pub(crate) struct WriteRequest {
    encoded: Encoded,
    done: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone)]
pub(crate) struct Writer {
    sender: mpsc::Sender<WriteRequest>,
    ids: Arc<AtomicU64>,
    outbound_feeds: OutboundFeeds,
}

impl Writer {
    fn new(sender: mpsc::Sender<WriteRequest>, outbound_feeds: OutboundFeeds) -> Self {
        Self {
            sender,
            ids: Arc::new(AtomicU64::new(1)),
            outbound_feeds,
        }
    }

    pub async fn send<T: Serialize + Send + 'static>(&self, value: T) -> Result<(), ProtocolError> {
        let (encoded, pending) = encode(&value, self.ids.clone())?;
        let (done, finished) = oneshot::channel();
        self.sender
            .send(WriteRequest { encoded, done })
            .await
            .map_err(|_| ProtocolError::InvalidFrame("protocol writer stopped".into()))?;
        finished
            .await
            .map_err(|_| ProtocolError::InvalidFrame("protocol writer stopped".into()))?
            .map_err(ProtocolError::InvalidFrame)?;
        for feed in pending {
            let abort = feed.receiver.activate(feed.id, self.clone());
            self.outbound_feeds
                .lock()
                .expect("outbound feed registry poisoned")
                .insert(feed.id, abort);
        }
        Ok(())
    }

    fn try_send<T: Serialize>(&self, value: T) {
        let Ok((encoded, pending)) = encode(&value, self.ids.clone()) else {
            return;
        };
        if !pending.is_empty() {
            return;
        }
        let (done, _finished) = oneshot::channel();
        let _ = self.sender.try_send(WriteRequest { encoded, done });
    }
}

pub(crate) fn writer_channel() -> (Writer, mpsc::Receiver<WriteRequest>, OutboundFeeds) {
    let (sender, receiver) = mpsc::channel(32);
    let outbound = Arc::new(Mutex::new(HashMap::new()));
    (Writer::new(sender, outbound.clone()), receiver, outbound)
}

pub(crate) async fn run_writer<W: AsyncWrite + Unpin>(
    mut connection: Connection<W>,
    mut requests: mpsc::Receiver<WriteRequest>,
) {
    while let Some(request) = requests.recv().await {
        let result = connection
            .send_encoded(request.encoded)
            .await
            .map_err(|error| error.to_string());
        let failed = result.is_err();
        let _ = request.done.send(result);
        if failed {
            break;
        }
    }
}

pub(crate) fn decode_context(feeds: InboundFeeds, writer: Writer) -> DecodeContext {
    DecodeContext { feeds, writer }
}

pub(crate) async fn route_feed(
    raw: RawMessage,
    feeds: &InboundFeeds,
    outbound_feeds: &OutboundFeeds,
) -> Result<Option<RawMessage>, ProtocolError> {
    let value: serde_json::Value =
        serde_json::from_slice(&raw.json).map_err(ProtocolError::Decode)?;
    let Some(id) = value.get("feed").and_then(serde_json::Value::as_u64) else {
        return Ok(Some(raw));
    };
    if value.get("cancel").is_some() {
        if let Some(feed) = outbound_feeds
            .lock()
            .expect("outbound feed registry poisoned")
            .remove(&id)
        {
            feed.abort();
        }
        return Ok(None);
    }
    if value.get("item").is_none() && value.get("end").is_none() {
        return Ok(Some(raw));
    }
    let Some(sender) = feeds
        .lock()
        .expect("inbound feed registry poisoned")
        .get(&id)
        .cloned()
    else {
        // The receiver may have been dropped while already-written items were
        // in flight. Cancellation is scoped to the feed, never the connection.
        return Ok(None);
    };
    if sender.send(raw).await.is_err() {
        feeds
            .lock()
            .expect("inbound feed registry poisoned")
            .remove(&id);
        return Ok(None);
    }
    if value.get("end").is_some() {
        feeds
            .lock()
            .expect("inbound feed registry poisoned")
            .remove(&id);
    }
    Ok(None)
}

pub(crate) fn inbound_feeds() -> InboundFeeds {
    Arc::new(Mutex::new(HashMap::new()))
}
