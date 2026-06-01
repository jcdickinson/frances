use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt, stream};
use serde::de::DeserializeOwned;
use tokio_stream::wrappers::WatchStream;

use crate::config::Configuration;
use crate::deserializer::ConfigDeserializer;
use crate::error::{ConfigBindError, MapError};
use crate::handle::BindingRegistry;
use crate::value::Path;

/// Pulls `T` out of a snapshot. `None` means the path is absent or
/// deserialisation failed; failures are logged inside the closure via
/// `tracing::warn!`.
pub(crate) type DeserializeFn<T> = Arc<dyn Fn(&Configuration) -> Option<T> + Send + Sync>;

/// Composed mapper chain that transforms `T → U`.
pub(crate) type MapperFn<T, U> =
    Arc<dyn Fn(T) -> BoxFuture<'static, Result<U, MapError>> + Send + Sync>;

/// Pure data + the deserialise step + the composed mapper chain. Cloneable
/// via `Arc`. Both Optional and Required forms share the same inner; the
/// wrapper type controls absence policy.
pub(crate) struct BindingInner<T, U> {
    pub(crate) path: Arc<str>,
    pub(crate) value: ArcSwapOption<U>,
    pub(crate) notify: tokio::sync::watch::Sender<u64>,
    pub(crate) deserialize: DeserializeFn<T>,
    pub(crate) mapper: MapperFn<T, U>,
}

impl<T, U> BindingInner<T, U> {
    fn new(
        path: Arc<str>,
        initial: Option<Arc<U>>,
        deserialize: DeserializeFn<T>,
        mapper: MapperFn<T, U>,
    ) -> Self {
        let (notify, _rx) = tokio::sync::watch::channel(0u64);
        Self {
            path,
            value: ArcSwapOption::new(initial),
            notify,
            deserialize,
            mapper,
        }
    }
}

/// Async binding-refresh trait. The handle's registry stores
/// `Weak<dyn BindingRefresh>` and awaits one refresh per registered binding
/// per applied event-batch.
#[async_trait]
pub(crate) trait BindingRefresh: Send + Sync {
    async fn refresh_from(&self, snapshot: Arc<Configuration>);
}

pub(crate) struct OptionalRefresher<T, U> {
    pub(crate) inner: Arc<BindingInner<T, U>>,
}

pub(crate) struct RequiredRefresher<T, U> {
    pub(crate) inner: Arc<BindingInner<T, U>>,
}

async fn compute_next<T, U>(inner: &BindingInner<T, U>, snapshot: &Configuration) -> Option<Arc<U>>
where
    T: Send + 'static,
    U: Send + Sync + 'static,
{
    let t = (inner.deserialize)(snapshot)?;
    match (inner.mapper)(t).await {
        Ok(u) => Some(Arc::new(u)),
        Err(e) => {
            tracing::warn!(path = %inner.path, error = %e, "mapper failed");
            None
        }
    }
}

#[async_trait]
impl<T, U> BindingRefresh for OptionalRefresher<T, U>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    async fn refresh_from(&self, snapshot: Arc<Configuration>) {
        let next = compute_next(&self.inner, &snapshot).await;
        self.inner.value.store(next);
        self.inner.notify.send_modify(|v| *v = v.wrapping_add(1));
    }
}

#[async_trait]
impl<T, U> BindingRefresh for RequiredRefresher<T, U>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    async fn refresh_from(&self, snapshot: Arc<Configuration>) {
        match compute_next(&self.inner, &snapshot).await {
            Some(next) => {
                self.inner.value.store(Some(next));
                self.inner.notify.send_modify(|v| *v = v.wrapping_add(1));
            }
            None => tracing::warn!(
                path = %self.inner.path,
                "required config path went absent or failed; retaining last value",
            ),
        }
    }
}

/// An optional binding. `T` is the type produced by serde from the
/// configuration tree; `U` is the type currently exposed to readers (initially
/// `T`, replaced by `map_async`).
pub struct ConfigBinding<T, U = T> {
    pub(crate) inner: Arc<BindingInner<T, U>>,
    pub(crate) refresher: Arc<OptionalRefresher<T, U>>,
    pub(crate) registry: Weak<BindingRegistry>,
}

/// A binding whose value is guaranteed to exist. Sticky on absence: if the
/// source path goes away or a mapper rejects a refresh, the previous value
/// is retained and subscribers are not notified.
pub struct RequiredConfigBinding<T, U = T> {
    pub(crate) inner: Arc<BindingInner<T, U>>,
    pub(crate) refresher: Arc<RequiredRefresher<T, U>>,
    pub(crate) registry: Weak<BindingRegistry>,
}

impl<T, U> Clone for ConfigBinding<T, U> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            refresher: self.refresher.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<T, U> Clone for RequiredConfigBinding<T, U> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            refresher: self.refresher.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<T, U: fmt::Debug> fmt::Debug for ConfigBinding<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigBinding")
            .field("path", &self.inner.path)
            .field("value", &self.inner.value.load_full())
            .finish()
    }
}

impl<T, U: fmt::Debug> fmt::Debug for RequiredConfigBinding<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequiredConfigBinding")
            .field("path", &self.inner.path)
            .field("value", &self.inner.value.load_full())
            .finish()
    }
}

// --------------------------------------------------------------------------
// Construction from snapshots / handles
// --------------------------------------------------------------------------

impl<T> ConfigBinding<T, T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    /// Build a binding from a snapshot of the configuration tree.
    ///
    /// `registry` is the live handle's binding registry; pass `Weak::new()`
    /// when building from a snapshot directly (refreshes will not fire, but
    /// transforms still work).
    pub(crate) fn from_snapshot(
        path: Path,
        config: Option<&Configuration>,
        registry: Weak<BindingRegistry>,
    ) -> Result<Self, ConfigBindError> {
        let path_str: Arc<str> = Arc::from(path.to_string());

        let path_for_de = path.clone();
        let path_str_de = path_str.clone();
        let deserialize: DeserializeFn<T> = Arc::new(move |snapshot: &Configuration| {
            let cursor = snapshot.get(path_for_de.clone());
            let node = cursor.config()?;
            let de = ConfigDeserializer::new(path_str_de.clone(), node);
            match T::deserialize(de) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        path = %path_str_de,
                        error = %e,
                        "config deserialise failed; treating as absent",
                    );
                    None
                }
            }
        });

        let mapper: MapperFn<T, T> =
            Arc::new(|t: T| Box::pin(async move { Ok(t) }) as BoxFuture<'static, _>);

        let initial: Option<Arc<T>> = match config {
            None => None,
            Some(node) => {
                let de = ConfigDeserializer::new(path_str.clone(), node);
                Some(Arc::new(T::deserialize(de)?))
            }
        };

        let inner = Arc::new(BindingInner::new(path_str, initial, deserialize, mapper));
        let refresher = Arc::new(OptionalRefresher {
            inner: inner.clone(),
        });
        if let Some(reg) = registry.upgrade() {
            reg.register(Arc::downgrade(&refresher) as Weak<dyn BindingRefresh>);
        }
        Ok(Self {
            inner,
            refresher,
            registry,
        })
    }
}

// --------------------------------------------------------------------------
// Optional API
// --------------------------------------------------------------------------

impl<T, U> ConfigBinding<T, U>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    pub fn path(&self) -> &str {
        &self.inner.path
    }

    /// Sync, lock-free read.
    pub fn get(&self) -> Option<ConfigBindingRef<U>> {
        self.inner
            .value
            .load_full()
            .map(|value| ConfigBindingRef { value })
    }

    /// Future changes only. Yields `Some(_)` when a value is set or replaced;
    /// yields `None` when the source goes absent or a mapper fails.
    pub fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Option<Arc<U>>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        Box::pin(WatchStream::from_changes(rx).map(move |_tick| inner.value.load_full()))
    }

    /// Same as [`subscribe`](Self::subscribe) but yields the current value
    /// as the first item before waiting for changes.
    pub fn subscribe_now(&self) -> Pin<Box<dyn Stream<Item = Option<Arc<U>>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        let initial = inner.value.load_full();
        let inner_for_map = inner.clone();
        let tail = WatchStream::from_changes(rx).map(move |_tick| inner_for_map.value.load_full());
        Box::pin(stream::iter(std::iter::once(initial)).chain(tail))
    }

    /// Promote to a `Required` form, asserting the value is currently set.
    pub fn required(self) -> Result<RequiredConfigBinding<T, U>, ConfigBindError> {
        if self.inner.value.load().is_none() {
            return Err(ConfigBindError::RequiredSection {
                path: self.inner.path.clone(),
            });
        }
        let refresher = Arc::new(RequiredRefresher {
            inner: self.inner.clone(),
        });
        if let Some(reg) = self.registry.upgrade() {
            reg.register(Arc::downgrade(&refresher) as Weak<dyn BindingRefresh>);
        }
        Ok(RequiredConfigBinding {
            inner: self.inner,
            refresher,
            registry: self.registry,
        })
        // self.refresher (the OptionalRefresher) drops here; its Weak in
        // the registry is GC'd on the next refresh's retain pass.
    }
}

impl<T, U> ConfigBinding<T, U>
where
    T: Send + Sync + 'static,
    U: Default + Send + Sync + 'static,
{
    /// Returns the current value or, if absent, replaces the slot with
    /// `U::default()` and returns that.
    pub fn get_or_default(&self) -> ConfigBindingRef<U> {
        if let Some(r) = self.get() {
            return r;
        }
        let default = Arc::new(U::default());
        self.inner.value.store(Some(default.clone()));
        ConfigBindingRef { value: default }
    }
}

// --------------------------------------------------------------------------
// map_async
// --------------------------------------------------------------------------

impl<T, U> ConfigBinding<T, U>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    /// Apply an async transform to produce `ConfigBinding<T, U_new>`.
    ///
    /// `f` is invoked once at construction on the current `U` (if any); if it
    /// errors, `map_async` returns `Err(MapError)`. After construction, `f`
    /// runs as the tail of the composed mapper chain on every refresh.
    ///
    /// Absence is a no-op: when deserialisation yields `None`, the mapper
    /// chain is not invoked and the mapped binding is `None`.
    ///
    /// `T` is preserved through the chain — only `U` changes.
    pub async fn map_async<UNew, F>(self, f: F) -> Result<ConfigBinding<T, UNew>, MapError>
    where
        UNew: Send + Sync + 'static,
        F: Fn(U) -> BoxFuture<'static, Result<UNew, MapError>> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        let old_mapper = self.inner.mapper.clone();
        let f_for_compose = f.clone();
        let composed: MapperFn<T, UNew> = Arc::new(move |t: T| {
            let old = old_mapper.clone();
            let f = f_for_compose.clone();
            Box::pin(async move {
                let u = old(t).await?;
                f(u).await
            })
        });

        // Compute the initial value by running the deserialise + composed
        // mapper pipeline against the current snapshot. A mapper error here
        // propagates as `Err(MapError)`. A None-from-deserialise (path
        // absent) is a no-op, leaving the mapped binding empty.
        let path_str = self.inner.path.clone();
        let initial: Option<Arc<UNew>> = match self.registry.upgrade() {
            Some(reg) => {
                let snap = reg.snapshot();
                match (self.inner.deserialize)(&snap) {
                    None => None,
                    Some(t) => Some(Arc::new(composed(t).await?)),
                }
            }
            None => None,
        };

        let inner = Arc::new(BindingInner::new(
            path_str,
            initial,
            self.inner.deserialize.clone(),
            composed,
        ));
        let refresher = Arc::new(OptionalRefresher {
            inner: inner.clone(),
        });
        if let Some(reg) = self.registry.upgrade() {
            reg.register(Arc::downgrade(&refresher) as Weak<dyn BindingRefresh>);
        }
        Ok(ConfigBinding {
            inner,
            refresher,
            registry: self.registry,
        })
    }
}

// --------------------------------------------------------------------------
// Required API
// --------------------------------------------------------------------------

impl<T, U> RequiredConfigBinding<T, U>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    pub fn path(&self) -> &str {
        &self.inner.path
    }

    /// Sync, lock-free read. Always succeeds — Required bindings are sticky
    /// on absence and never observe a `None` after promotion.
    pub fn get(&self) -> ConfigBindingRef<U> {
        ConfigBindingRef {
            value: self
                .inner
                .value
                .load_full()
                .expect("required binding is sticky on absence"),
        }
    }

    /// Future changes only. Required's stream never emits absence — refresh
    /// silently retains the last value when the source goes missing or a
    /// mapper rejects.
    pub fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<U>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        Box::pin(WatchStream::from_changes(rx).map(move |_tick| {
            inner
                .value
                .load_full()
                .expect("required binding is sticky on absence")
        }))
    }

    /// Same as [`subscribe`](Self::subscribe) but yields the current value
    /// as the first item before waiting for changes.
    pub fn subscribe_now(&self) -> Pin<Box<dyn Stream<Item = Arc<U>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        let initial = inner
            .value
            .load_full()
            .expect("required binding is sticky on absence");
        let inner_for_map = inner.clone();
        let tail = WatchStream::from_changes(rx).map(move |_tick| {
            inner_for_map
                .value
                .load_full()
                .expect("required binding is sticky on absence")
        });
        Box::pin(stream::iter(std::iter::once(initial)).chain(tail))
    }
}

// --------------------------------------------------------------------------
// Read guard
// --------------------------------------------------------------------------

/// A snapshot of a binding's current value. Derefs to `&U`.
pub struct ConfigBindingRef<U> {
    value: Arc<U>,
}

impl<U: fmt::Debug> fmt::Debug for ConfigBindingRef<U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ConfigBindingRef")
            .field(&self.deref())
            .finish()
    }
}

impl<U> Deref for ConfigBindingRef<U> {
    type Target = U;

    fn deref(&self) -> &U {
        &self.value
    }
}
