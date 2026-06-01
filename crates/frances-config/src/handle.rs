use std::sync::{Arc, Weak};

use arc_swap::{ArcSwap, Guard};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::binding::{BindingRefresh, ConfigBinding};
use crate::config::Configuration;
use crate::error::{BuildError, ConfigBindError};
use crate::event::{EventSender, InternalEvent, ProviderId};
use crate::provider::ConfigProvider;
use crate::value::Path;

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
        let mut g = self.bindings.lock();
        g.push(w);
    }

    /// Lock-free snapshot read.
    pub(crate) fn snapshot(&self) -> Arc<Configuration> {
        self.snapshot.load_full()
    }

    async fn refresh_all(&self, snapshot: &Arc<Configuration>) {
        let alive: Vec<Arc<dyn BindingRefresh>> = {
            let mut g = self.bindings.lock();
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
/// background task that consumes batched [`ConfigEvent`](crate::ConfigEvent)s, rebuilds the
/// snapshot, and refreshes registered bindings.
#[derive(Clone)]
pub struct ConfigHandle {
    registry: Arc<BindingRegistry>,
    _providers: Arc<Vec<Arc<dyn ConfigProvider>>>,
}

impl ConfigHandle {
    /// Build a handle from a list of providers.
    ///
    /// Providers are loaded **sequentially**. Each provider gets its own
    /// layer; later providers in the vec have higher priority. After all
    /// providers' `load()` futures resolve, a barrier is sent through the
    /// same channel and `build` returns once it has been processed,
    /// guaranteeing the snapshot reflects every initial event.
    pub async fn build(providers: Vec<Arc<dyn ConfigProvider>>) -> Result<Self, BuildError> {
        let num_layers = providers.len();

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
            _providers: Arc::new(providers),
        })
    }

    /// Lock-free read of the current snapshot.
    pub fn snapshot(&self) -> Guard<Arc<Configuration>> {
        self.registry.snapshot.load()
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

    /// View this handle through a fixed path prefix. Every
    /// [`ConfigHandleRef::bind`] prepends `prefix` before delegating to
    /// [`bind`](Self::bind).
    pub fn scoped(&self, prefix: impl Into<Path>) -> ConfigHandleRef<'_> {
        ConfigHandleRef {
            handle: self,
            prefix: prefix.into(),
        }
    }
}

/// A view onto a [`ConfigHandle`] with a fixed path prefix.
///
/// The prefix is "optimistic" — it does not have to currently resolve in
/// the snapshot. Bindings tolerate absence and re-resolve from the prefix
/// on every refresh, so subtree-scoped consumers can be wired up before
/// (or independently of) the providers that populate them.
pub struct ConfigHandleRef<'a> {
    handle: &'a ConfigHandle,
    prefix: Path,
}

impl<'a> ConfigHandleRef<'a> {
    /// Bind `prefix + path` against the underlying handle.
    pub fn bind<T>(&self, path: impl Into<Path>) -> Result<ConfigBinding<T, T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        self.handle.bind(self.prefix.join(path))
    }

    /// Return a deeper-scoped view by appending `path` to the prefix.
    pub fn get(&self, path: impl Into<Path>) -> ConfigHandleRef<'a> {
        ConfigHandleRef {
            handle: self.handle,
            prefix: self.prefix.join(path),
        }
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
