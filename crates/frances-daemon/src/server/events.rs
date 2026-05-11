use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{trace, warn};

use crate::protocol::PromptId;
use crate::transport::read_message;

use super::ServerState;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct EventsRouter {
    inner: DashMap<PromptId, EventsSlot>,
}

enum EventsSlot {
    HasStream(UnixStream),
    Waiting(oneshot::Sender<UnixStream>),
}

impl EventsRouter {
    fn register(&self, id: PromptId, stream: UnixStream) {
        match self.inner.entry(id) {
            Entry::Occupied(mut occ) => match occ.get() {
                EventsSlot::Waiting(_) => {
                    if let EventsSlot::Waiting(tx) = occ.remove() {
                        let _ = tx.send(stream);
                    }
                }
                EventsSlot::HasStream(_) => {
                    occ.insert(EventsSlot::HasStream(stream));
                }
            },
            Entry::Vacant(vac) => {
                vac.insert(EventsSlot::HasStream(stream));
            }
        }
    }

    pub(super) async fn take(&self, id: PromptId) -> Option<UnixStream> {
        let rx = match self.inner.entry(id) {
            Entry::Occupied(occ) => match occ.remove() {
                EventsSlot::HasStream(s) => return Some(s),
                EventsSlot::Waiting(_) => return None,
            },
            Entry::Vacant(vac) => {
                let (tx, rx) = oneshot::channel();
                vac.insert(EventsSlot::Waiting(tx));
                rx
            }
        };
        match tokio::time::timeout(EVENTS_PAIRING_TIMEOUT, rx).await {
            Ok(Ok(stream)) => Some(stream),
            _ => {
                self.inner.remove(&id);
                None
            }
        }
    }
}

pub(super) async fn accept_events(listener: UnixListener, state: Arc<ServerState>) {
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let id: PromptId = match read_message(&mut stream).await {
                        Ok(id) => id,
                        Err(error) => {
                            warn!(%error, "events handshake failed");
                            return;
                        }
                    };
                    trace!(prompt_id = %id, "events socket registered");
                    state.events.register(id, stream);
                });
            }
            Err(error) => {
                warn!(%error, "events accept error");
                return;
            }
        }
    }
}
