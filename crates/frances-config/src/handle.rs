use std::sync::{Arc, Mutex, Weak};

use arc_swap::{ArcSwap, Guard};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::binding::{BindingRefresh, ConfigBinding};
use crate::config::Configuration;
use crate::error::{BuildError, ConfigBindError, ReloadError};
use crate::event::{ConfigEvent, EventSender, InternalEvent, ProviderId};
use crate::provider::ConfigProvider;
use crate::value::Path;

/// Default capacity for the internal event channel between providers and the
/// processor task. Tuned generously — startup events from a few providers
/// fit comfortably; runtime events are infrequent.
const EVENT_BUFFER: usize = 1024;

/// Registry of bindings driven by a [`ConfigHandle`].
///
/// Bindings register a `Weak<dyn BindingRefresh>` here when created via the
/// handle (and re-register when transformed via `required` / `map_async`).
/// On every applied event-batch the registry's `refresh_all` walks every
/// live binding once.
pub struct BindingRegistry {
    bindings: Mutex<Vec<Weak<dyn BindingRefresh>>>,
    snapshot: Arc<ArcSwap<Configuration>>,
}

impl BindingRegistry {
    fn new(snapshot: Arc<ArcSwap<Configuration>>) -> Self {
        Self {
            bindings: Mutex::new(Vec::new()),
            snapshot,
        }
    }

    pub(crate) fn register(&self, w: Weak<dyn BindingRefresh>) {
        let mut g = self.bindings.lock().expect("registry mutex poisoned");
        g.push(w);
    }

    /// Lock-free snapshot read. Used by `map_async` at construction time to
    /// run the freshly-composed mapper chain against the current
    /// configuration.
    pub(crate) fn snapshot(&self) -> Arc<Configuration> {
        self.snapshot.load_full()
    }

    async fn refresh_all(&self, snapshot: &Arc<Configuration>) {
        let alive: Vec<Arc<dyn BindingRefresh>> = {
            let mut g = self.bindings.lock().expect("registry mutex poisoned");
            g.retain(|w| w.strong_count() > 0);
            g.iter().filter_map(|w| w.upgrade()).collect()
        };
        for r in alive {
            r.refresh_from(snapshot.clone()).await;
        }
    }
}

/// A live configuration root.
///
/// Holds the current [`Configuration`] snapshot in an [`ArcSwap`], owns the
/// providers (so they can keep publishing runtime events), and runs a
/// background task that consumes batched [`ConfigEvent`]s, rebuilds the
/// snapshot, and refreshes registered bindings.
#[derive(Clone)]
pub struct ConfigHandle {
    registry: Arc<BindingRegistry>,
    events_tx: mpsc::Sender<InternalEvent>,
    /// `ProviderId` reserved for [`ConfigHandle::publish`]; sits above all
    /// providers in priority.
    manual_id: ProviderId,
    /// Keeps providers alive for the lifetime of the handle so they can
    /// continue publishing runtime events.
    _providers: Arc<Vec<Arc<dyn ConfigProvider>>>,
}

impl ConfigHandle {
    /// Build a handle from a list of providers.
    ///
    /// Providers are loaded **sequentially**. Each provider gets its own
    /// layer; later providers in the vec have higher priority. An additional
    /// "manual" layer sits above all providers and is the destination for
    /// [`ConfigHandle::publish`]. After all providers' `load()` futures
    /// resolve, a barrier is sent through the same channel and `build`
    /// returns once it has been processed, guaranteeing the snapshot
    /// reflects every initial event.
    pub async fn build(providers: Vec<Arc<dyn ConfigProvider>>) -> Result<Self, BuildError> {
        let num_layers = providers.len() + 1;
        let manual_id = ProviderId(providers.len());

        let snapshot: Arc<ArcSwap<Configuration>> =
            Arc::new(ArcSwap::from_pointee(Configuration::empty(num_layers)));
        let registry = Arc::new(BindingRegistry::new(snapshot.clone()));
        let (events_tx, events_rx) = mpsc::channel::<InternalEvent>(EVENT_BUFFER);

        spawn_processor(events_rx, snapshot.clone(), registry.clone(), num_layers);

        for (i, p) in providers.iter().enumerate() {
            let sender = EventSender {
                inner: events_tx.clone(),
                provider_id: ProviderId(i),
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
            registry,
            events_tx,
            manual_id,
            _providers: Arc::new(providers),
        })
    }

    /// Lock-free read of the current snapshot.
    pub fn snapshot(&self) -> Guard<Arc<Configuration>> {
        self.registry.snapshot.load()
    }

    /// Manually publish a batch of events. Events go into the reserved
    /// manual layer, which sits above all provider layers.
    pub async fn publish(&self, events: Vec<ConfigEvent>) -> Result<(), ReloadError> {
        self.events_tx
            .send(InternalEvent::Batch {
                provider_id: self.manual_id,
                events,
            })
            .await
            .map_err(|_| ReloadError::ProcessorGone)
    }

    /// Bind a typed view at `path`. Registered with the handle so future
    /// events re-deserialize this binding automatically.
    pub fn bind<T>(&self, path: impl Into<Path>) -> Result<ConfigBinding<T, T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        let path = path.into();
        let snapshot = self.registry.snapshot();
        let cursor = snapshot.get(path.clone());
        ConfigBinding::<T, T>::from_snapshot(path, cursor.config(), Arc::downgrade(&self.registry))
    }
}

fn spawn_processor(
    mut events_rx: mpsc::Receiver<InternalEvent>,
    snapshot: Arc<ArcSwap<Configuration>>,
    registry: Arc<BindingRegistry>,
    num_layers: usize,
) {
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                InternalEvent::Batch {
                    provider_id,
                    events,
                } => {
                    let current = snapshot.load_full();
                    let next = current
                        .applied_batch(provider_id, &events)
                        .unwrap_or_else(|| Configuration::empty(num_layers));
                    let next = Arc::new(next);
                    snapshot.store(next.clone());
                    registry.refresh_all(&next).await;
                }
                InternalEvent::Barrier(tx) => {
                    let _ = tx.send(());
                }
            }
        }
    });
}
