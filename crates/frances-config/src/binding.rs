use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use arc_swap::{ArcSwapOption, Guard};
use futures::{Stream, StreamExt, stream};
use serde::de::DeserializeOwned;
use tokio_stream::wrappers::WatchStream;

use crate::config::Configuration;
use crate::deserializer::ConfigDeserializer;
use crate::error::ConfigBindError;
use crate::value::Path;

const KIND_OPTIONAL: u8 = 0;
const KIND_REQUIRED: u8 = 1;

/// Marker for bindings that may be missing.
pub struct Optional;

/// Marker for bindings whose value is guaranteed to exist.
pub struct Required;

/// Type alias for [`ConfigBinding`] in its required form.
pub type RequiredConfigBinding<T> = ConfigBinding<T, Required>;

pub struct ConfigBinding<T, K = Optional> {
    path: Arc<str>,
    pub(crate) inner: Arc<BindingInner<T>>,
    _kind: PhantomData<K>,
}

type RecomputeFn<T> =
    Box<dyn Fn(&Configuration) -> Result<Option<Arc<T>>, ConfigBindError> + Send + Sync>;

pub(crate) struct BindingInner<T> {
    pub(crate) path: Arc<str>,
    pub(crate) value: ArcSwapOption<T>,
    /// `KIND_OPTIONAL` or `KIND_REQUIRED`. Mutable so `Optional::required()`
    /// can flip it without rebuilding the inner.
    pub(crate) kind: AtomicU8,
    pub(crate) notify: tokio::sync::watch::Sender<u64>,
    pub(crate) recompute: RecomputeFn<T>,
}

pub(crate) trait BindingRefresh: Send + Sync {
    fn refresh_from(&self, snapshot: &Configuration);
}

impl<T> BindingRefresh for BindingInner<T>
where
    T: Send + Sync + 'static,
{
    fn refresh_from(&self, snapshot: &Configuration) {
        match (self.recompute)(snapshot) {
            Ok(Some(next)) => {
                self.value.store(Some(next));
                self.notify.send_modify(|v| *v = v.wrapping_add(1));
            }
            Ok(None) => match self.kind.load(Ordering::Relaxed) {
                KIND_REQUIRED => {
                    tracing::warn!(
                        path = %self.path,
                        "required config path went absent; retaining last value",
                    );
                }
                _ => {
                    self.value.store(None);
                    self.notify.send_modify(|v| *v = v.wrapping_add(1));
                }
            },
            Err(e) => tracing::warn!(
                path = %self.path,
                error = %e,
                "binding refresh failed; retaining previous value",
            ),
        }
    }
}

impl<T, K> Clone for ConfigBinding<T, K> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            inner: self.inner.clone(),
            _kind: PhantomData,
        }
    }
}

impl<T: fmt::Debug, K> fmt::Debug for ConfigBinding<T, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigBinding")
            .field("path", &self.path)
            .field("value", &self.inner.value.load_full())
            .finish()
    }
}

impl<T> ConfigBinding<T, Optional>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    /// Build an optional binding from a snapshot of the configuration tree.
    /// Used by [`Configuration::bind`] and [`ConfigurationRef::bind`].
    pub(crate) fn from_snapshot(
        path: Path,
        config: Option<&Configuration>,
    ) -> Result<Self, ConfigBindError> {
        let path_str: Arc<str> = Arc::from(path.to_string());
        let path_for_closure = path.clone();
        let path_str_closure = path_str.clone();
        let recompute = Box::new(move |snapshot: &Configuration| {
            let cursor = snapshot.get(path_for_closure.clone());
            match cursor.config() {
                None => Ok::<Option<Arc<T>>, ConfigBindError>(None),
                Some(node) => {
                    let de = ConfigDeserializer::new(path_str_closure.clone(), node);
                    T::deserialize(de).map(|v| Some(Arc::new(v)))
                }
            }
        });
        let initial = match config {
            None => None,
            Some(node) => {
                let de = ConfigDeserializer::new(path_str.clone(), node);
                Some(Arc::new(T::deserialize(de)?))
            }
        };
        let (notify, _rx) = tokio::sync::watch::channel(0u64);
        let inner = Arc::new(BindingInner {
            path: path_str.clone(),
            value: ArcSwapOption::new(initial),
            kind: AtomicU8::new(KIND_OPTIONAL),
            notify,
            recompute,
        });
        Ok(Self {
            path: path_str,
            inner,
            _kind: PhantomData,
        })
    }
}

impl<T> ConfigBinding<T, Optional>
where
    T: Send + Sync + 'static,
{
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Sync, lock-free read.
    pub fn get(&self) -> Option<ConfigBindingRef<T>> {
        let guard = self.inner.value.load();
        if guard.is_some() {
            Some(ConfigBindingRef { guard })
        } else {
            None
        }
    }

    /// Future changes only. Yields `Some(_)` when a value is set or replaced;
    /// yields `None` when the source path goes absent.
    pub fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Option<Arc<T>>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        Box::pin(WatchStream::from_changes(rx).map(move |_tick| inner.value.load_full()))
    }

    /// Same as [`subscribe`](Self::subscribe) but yields the current value
    /// as the first item before waiting for changes.
    pub fn subscribe_now(&self) -> Pin<Box<dyn Stream<Item = Option<Arc<T>>> + Send>> {
        let inner = self.inner.clone();
        let rx = self.inner.notify.subscribe();
        let initial = inner.value.load_full();
        let inner_for_map = inner.clone();
        let tail = WatchStream::from_changes(rx).map(move |_tick| inner_for_map.value.load_full());
        Box::pin(stream::iter(std::iter::once(initial)).chain(tail))
    }
}

impl<T> ConfigBinding<T, Optional>
where
    T: Default + Send + Sync + 'static,
{
    /// Returns the current value or, if absent, replaces the slot with
    /// `T::default()` and returns that.
    pub fn get_or_default(&self) -> ConfigBindingRef<T> {
        if let Some(r) = self.get() {
            return r;
        }
        let default = Arc::new(T::default());
        self.inner.value.store(Some(default));
        ConfigBindingRef {
            guard: self.inner.value.load(),
        }
    }
}

impl<T> ConfigBinding<T, Optional>
where
    T: Send + Sync + 'static,
{
    /// Promote this binding to a `Required` form, asserting the value is
    /// currently set. Flips the inner's kind flag so future refreshes become
    /// sticky on absence.
    pub fn required(self) -> Result<ConfigBinding<T, Required>, ConfigBindError> {
        if self.inner.value.load().is_some() {
            self.inner.kind.store(KIND_REQUIRED, Ordering::Relaxed);
            Ok(ConfigBinding {
                path: self.path,
                inner: self.inner,
                _kind: PhantomData,
            })
        } else {
            Err(ConfigBindError::RequiredSection { path: self.path })
        }
    }
}

impl<T> ConfigBinding<T, Required>
where
    T: Send + Sync + 'static,
{
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Sync, lock-free read. Always succeeds — the value is guaranteed to be
    /// present at construction, and Required bindings are sticky on absence.
    pub fn get(&self) -> ConfigBindingRef<T> {
        ConfigBindingRef {
            guard: self.inner.value.load(),
        }
    }

    /// Map this binding through `f` to produce a derived `Required` binding.
    /// The mapper re-runs whenever the source binding refreshes.
    ///
    /// A background task is spawned that watches the upstream's notify
    /// channel; when the upstream binding is dropped (its `Arc<BindingInner>`
    /// has no remaining strong refs), the task exits.
    pub fn map<U, F>(self, f: F) -> ConfigBinding<U, Required>
    where
        U: Send + Sync + 'static,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        let initial = self.inner.value.load_full().map(|v| Arc::new(f(&v)));
        let (notify, _rx) = tokio::sync::watch::channel(0u64);
        let derived_inner = Arc::new(BindingInner {
            path: self.path.clone(),
            value: ArcSwapOption::new(initial),
            kind: AtomicU8::new(KIND_REQUIRED),
            notify,
            // Mapped bindings don't refresh from snapshot — they refresh
            // via the watcher task wired below. The recompute closure is
            // kept as a no-op so the trait shape stays uniform.
            recompute: Box::new(|_| Ok(None)),
        });

        // Spawn a forwarder. Holds a strong ref to upstream (so its notify
        // channel stays alive) and a weak ref to derived. When derived dies,
        // upgrade fails and the task exits.
        let upstream = self.inner.clone();
        let derived_weak = Arc::downgrade(&derived_inner);
        let mapper = f.clone();
        let mut up_rx = self.inner.notify.subscribe();
        tokio::spawn(async move {
            while up_rx.changed().await.is_ok() {
                let Some(derived) = derived_weak.upgrade() else {
                    return;
                };
                let next = upstream.value.load_full().map(|v| Arc::new(mapper(&v)));
                if next.is_some() {
                    derived.value.store(next);
                    derived.notify.send_modify(|v| *v = v.wrapping_add(1));
                }
            }
        });

        ConfigBinding {
            path: self.path,
            inner: derived_inner,
            _kind: PhantomData,
        }
    }

    /// Future changes only. Required's stream never emits absence — refresh
    /// silently skips when the source path goes missing (see
    /// [`BindingRefresh`] for details).
    pub fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<T>> + Send>> {
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
    pub fn subscribe_now(&self) -> Pin<Box<dyn Stream<Item = Arc<T>> + Send>> {
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

/// A lock-free read guard for a binding's current value. Derefs to `&T`.
pub struct ConfigBindingRef<T> {
    guard: Guard<Option<Arc<T>>>,
}

impl<T: fmt::Debug> fmt::Debug for ConfigBindingRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ConfigBindingRef")
            .field(&self.deref())
            .finish()
    }
}

impl<T> Deref for ConfigBindingRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self.guard.as_ref() {
            Some(v) => v.as_ref(),
            None => unreachable!("ConfigBindingRef constructed only when value is Some"),
        }
    }
}
