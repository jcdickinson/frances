use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{trace, warn};

use crate::protocol::PromptId;
use crate::transport::read_message;

use super::ServerState;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct EventsRouter {
    inner: StdMutex<HashMap<PromptId, EventsSlot>>,
}

enum EventsSlot {
    HasStream(UnixStream),
    Waiting(oneshot::Sender<UnixStream>),
}

impl EventsRouter {
    fn register(&self, id: PromptId, stream: UnixStream) {
        let mut inner = self.inner.lock().expect("events router poisoned");
        match inner.remove(&id) {
            Some(EventsSlot::Waiting(tx)) => {
                let _ = tx.send(stream);
            }
            Some(EventsSlot::HasStream(_)) | None => {
                inner.insert(id, EventsSlot::HasStream(stream));
            }
        }
    }

    pub(super) async fn take(&self, id: PromptId) -> Option<UnixStream> {
        let rx = {
            let mut inner = self.inner.lock().expect("events router poisoned");
            match inner.remove(&id) {
                Some(EventsSlot::HasStream(s)) => return Some(s),
                Some(EventsSlot::Waiting(_)) => return None,
                None => {
                    let (tx, rx) = oneshot::channel();
                    inner.insert(id, EventsSlot::Waiting(tx));
                    rx
                }
            }
        };
        match tokio::time::timeout(EVENTS_PAIRING_TIMEOUT, rx).await {
            Ok(Ok(stream)) => Some(stream),
            _ => {
                self.inner
                    .lock()
                    .expect("events router poisoned")
                    .remove(&id);
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
