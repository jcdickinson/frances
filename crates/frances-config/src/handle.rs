use std::sync::{Arc, Mutex, Weak};

use arc_swap::{ArcSwap, Guard};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::binding::{BindingRefresh, ConfigBinding};
use crate::config::Configuration;
use crate::error::{BuildError, ConfigBindError, ReloadError};
use crate::event::{ConfigEvent, EventSender, InternalEvent};
use crate::provider::ConfigProvider;
use crate::value::Path;

/// Default capacity for the internal event channel between providers and the
/// processor task. Tuned generously — startup events from a few providers
/// fit comfortably; runtime events are infrequent.
const EVENT_BUFFER: usize = 1024;

/// A live configuration root.
///
/// Holds the current [`Configuration`] snapshot in an [`ArcSwap`], owns the
/// providers (so they can keep publishing runtime events), and runs a
/// background task that consumes [`ConfigEvent`]s, rebuilds the snapshot,
/// and refreshes registered bindings.
#[derive(Clone)]
pub struct ConfigHandle {
    snapshot: Arc<ArcSwap<Configuration>>,
    bindings: Arc<Mutex<Vec<Weak<dyn BindingRefresh>>>>,
    events_tx: mpsc::Sender<InternalEvent>,
    /// Keeps providers alive for the lifetime of the handle so they can
    /// continue publishing runtime events.
    _providers: Arc<Vec<Arc<dyn ConfigProvider>>>,
}

impl ConfigHandle {
    /// Build a handle from a list of providers.
    ///
    /// Providers are loaded **sequentially** — events from later providers
    /// arrive after, and therefore override, events from earlier ones. After
    /// all providers' `load()` futures have resolved, a barrier is sent
    /// through the same channel and `build` returns once it has been
    /// processed, guaranteeing the snapshot reflects every initial event.
    pub async fn build(providers: Vec<Arc<dyn ConfigProvider>>) -> Result<Self, BuildError> {
        let snapshot: Arc<ArcSwap<Configuration>> =
            Arc::new(ArcSwap::from_pointee(Configuration::default()));
        let bindings: Arc<Mutex<Vec<Weak<dyn BindingRefresh>>>> = Arc::new(Mutex::new(Vec::new()));
        let (events_tx, events_rx) = mpsc::channel::<InternalEvent>(EVENT_BUFFER);

        spawn_processor(events_rx, snapshot.clone(), bindings.clone());

        for p in &providers {
            let sender = EventSender {
                inner: events_tx.clone(),
            };
            p.load(sender).await?;
        }

        let (barrier_tx, barrier_rx) = oneshot::channel();
        events_tx
            .send(InternalEvent::Barrier(barrier_tx))
            .await
            .map_err(|_| BuildError::ProcessorGone)?;
        barrier_rx.await.map_err(|_| BuildError::ProcessorGone)?;

        Ok(Self {
            snapshot,
            bindings,
            events_tx,
            _providers: Arc::new(providers),
        })
    }

    /// Lock-free read of the current snapshot.
    pub fn snapshot(&self) -> Guard<Arc<Configuration>> {
        self.snapshot.load()
    }

    /// Manually replay an event. Useful for tests or for sources that don't
    /// fit the [`ConfigProvider`] shape.
    pub async fn publish(&self, event: ConfigEvent) -> Result<(), ReloadError> {
        self.events_tx
            .send(InternalEvent::Public(event))
            .await
            .map_err(|_| ReloadError::ProcessorGone)
    }

    /// Bind a typed view at `path`. Registered with the handle so future
    /// events re-deserialize this binding automatically.
    pub fn bind<T>(&self, path: impl Into<Path>) -> Result<ConfigBinding<T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        let path = path.into();
        let snapshot = self.snapshot.load_full();
        let binding = snapshot.get(path).bind::<T>()?;
        let strong: Arc<dyn BindingRefresh> = binding.inner.clone();
        let weak: Weak<dyn BindingRefresh> = Arc::downgrade(&strong);
        let mut guard = self
            .bindings
            .lock()
            .expect("binding registry mutex poisoned");
        guard.push(weak);
        Ok(binding)
    }
}

fn spawn_processor(
    mut events_rx: mpsc::Receiver<InternalEvent>,
    snapshot: Arc<ArcSwap<Configuration>>,
    bindings: Arc<Mutex<Vec<Weak<dyn BindingRefresh>>>>,
) {
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                InternalEvent::Public(ev) => {
                    let current = snapshot.load_full();
                    let next = Arc::new(current.applied(ev));
                    snapshot.store(next.clone());
                    refresh_bindings(&bindings, &next);
                }
                InternalEvent::Barrier(tx) => {
                    let _ = tx.send(());
                }
            }
        }
    });
}

fn refresh_bindings(bindings: &Mutex<Vec<Weak<dyn BindingRefresh>>>, snapshot: &Configuration) {
    let mut guard = bindings.lock().expect("binding registry mutex poisoned");
    guard.retain(|w| {
        if let Some(strong) = w.upgrade() {
            strong.refresh_from(snapshot);
            true
        } else {
            false
        }
    });
}
