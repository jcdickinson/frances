//! Lazy, config-driven cache of constructed `ErasedProvider`s.
//!
//! `new(handle)` binds to `model_providers` (id set) but doesn't build
//! anything. `get(id)` walks config-driven adds/removes since the last
//! call, returns the cached entry if present, and otherwise runs
//! `try_build(id)` to read `model_providers::<id>` and instantiate. Each
//! entry owns subscribe streams for its provider config + extras so the
//! next `get` can rebuild on config change.
//!
//! What's NOT here: an external factory-map / `EntryFactory` trait. The
//! impl-per-kind selection is a hard-coded match arm inside
//! `try_build` (today: a single OpenAI arm). Adding a new provider
//! is a one-line change here.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use dashmap::DashMap;
use frances_config::{ConfigBindError, ConfigBinding, ConfigHandle, Keys};
use futures::Stream;
use futures::StreamExt;
use futures::task::noop_waker_ref;
use parking_lot::Mutex;
use thiserror::Error;
use tracing::warn;

use frances_models_llm::config::{ProviderConfig, ResponsesModelExtras};
use frances_models_llm::wire::ErasedError;

use crate::provider::{ErasedProvider, Provider, erase};
use crate::providers::openai;

#[derive(Debug, Error)]
pub enum ProviderCacheError {
    #[error("bind {path}: {source}")]
    Bind {
        path: &'static str,
        #[source]
        source: ConfigBindError,
    },
    #[error("bind model_providers::{id}: {source}")]
    BindProvider {
        id: String,
        #[source]
        source: ConfigBindError,
    },
    #[error("bind model_provider_extensions::{id}: {source}")]
    BindExtensions {
        id: String,
        #[source]
        source: ConfigBindError,
    },
    #[error("ProviderConfig missing for current id")]
    ProviderConfigMissing,
    #[error("provider build: {0}")]
    Build(#[source] ErasedError),
}

/// Clone-by-value handle. Cheap to share; inner state is `Arc<Inner>`.
#[derive(Clone)]
pub struct ProviderCache {
    inner: Arc<Inner>,
}

struct Inner {
    handle: ConfigHandle,
    keys: ConfigBinding<Keys>,
    keys_stream: Mutex<KeysStream>,
    last_keys: Mutex<Keys>,
    entries: DashMap<String, Mutex<Entry>>,
}

type KeysStream = Pin<Box<dyn Stream<Item = Option<Arc<Keys>>> + Send>>;

struct Entry {
    provider: Arc<ErasedProvider>,
    refresh: RefreshFn,
}

type RefreshFn = Box<dyn FnMut() -> Option<Arc<ErasedProvider>> + Send>;

impl Entry {
    fn current(&mut self) -> Arc<ErasedProvider> {
        if let Some(new) = (self.refresh)() {
            self.provider = new;
        }
        self.provider.clone()
    }
}

impl ProviderCache {
    pub fn new(handle: ConfigHandle) -> std::result::Result<Self, ProviderCacheError> {
        let keys =
            handle
                .bind::<Keys>("model_providers")
                .map_err(|source| ProviderCacheError::Bind {
                    path: "model_providers",
                    source,
                })?;
        // subscribe_now seeds the stream with the current snapshot so the
        // first `refresh_id_set` learns the existing key set.
        let keys_stream = keys.subscribe_now();
        Ok(Self {
            inner: Arc::new(Inner {
                handle,
                keys,
                keys_stream: Mutex::new(keys_stream),
                last_keys: Mutex::new(Keys::default()),
                entries: DashMap::new(),
            }),
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<ErasedProvider>> {
        self.refresh_id_set();
        let id_lc = id.to_ascii_lowercase();
        if let Some(em) = self.inner.entries.get(&id_lc) {
            return Some(em.lock().current());
        }
        self.try_build(&id_lc)
    }

    fn refresh_id_set(&self) {
        let fired = {
            let mut s = self.inner.keys_stream.lock();
            drain_stream(&mut *s)
        };
        if !fired {
            return;
        }
        let new_keys: Keys = self
            .inner
            .keys
            .get()
            .map(|g| (*g).clone())
            .unwrap_or_default();
        let mut last = self.inner.last_keys.lock();
        let diff = new_keys.diff(&last);
        *last = new_keys;
        drop(last);
        if diff.removed.is_empty() {
            return;
        }
        for id in &diff.removed {
            self.inner.entries.remove(&id.to_ascii_lowercase());
        }
    }

    /// Build a new entry for `id_lc`. Hard-coded match-per-kind: today
    /// every id resolves to the OpenAI wire impl. Adding a new provider
    /// is a one-line addition here (key it on a provider-name field on
    /// `ProviderConfig` once that exists).
    fn try_build(&self, id_lc: &str) -> Option<Arc<ErasedProvider>> {
        let entry = match build_openai_entry(&self.inner.handle, id_lc) {
            Ok(entry) => entry,
            Err(e) => {
                warn!(provider = %id_lc, error = %e, "provider build failed");
                return None;
            }
        };
        let provider = entry.provider.clone();
        self.inner
            .entries
            .insert(id_lc.to_owned(), Mutex::new(entry));
        Some(provider)
    }
}

fn build_openai_entry(
    handle: &ConfigHandle,
    id: &str,
) -> std::result::Result<Entry, ProviderCacheError> {
    let pc = handle
        .bind::<ProviderConfig>(["model_providers", id])
        .map_err(|source| ProviderCacheError::BindProvider {
            id: id.to_owned(),
            source,
        })?;
    let ex = handle
        .bind::<ResponsesModelExtras>(["model_provider_extensions", id])
        .map_err(|source| ProviderCacheError::BindExtensions {
            id: id.to_owned(),
            source,
        })?;

    let initial = build_provider::<openai::Provider>(&pc, &ex)?;

    let mut pc_stream = pc.subscribe();
    let mut ex_stream = ex.subscribe();
    let pc_for_refresh = pc.clone();
    let ex_for_refresh = ex.clone();
    let refresh: RefreshFn = Box::new(move || {
        let pc_fired = drain_stream(&mut pc_stream);
        let ex_fired = drain_stream(&mut ex_stream);
        if !pc_fired && !ex_fired {
            return None;
        }
        match build_provider::<openai::Provider>(&pc_for_refresh, &ex_for_refresh) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(error = %e, "provider rebuild failed; retaining previous");
                None
            }
        }
    });

    Ok(Entry {
        provider: initial,
        refresh,
    })
}

fn build_provider<P>(
    pc: &ConfigBinding<ProviderConfig>,
    ex: &ConfigBinding<P::Extras>,
) -> std::result::Result<Arc<ErasedProvider>, ProviderCacheError>
where
    P: Provider + 'static,
    P::BuildError: Into<ErasedError>,
    P::Error: Into<ErasedError> + From<ErasedError>,
{
    let cfg = pc
        .get()
        .ok_or(ProviderCacheError::ProviderConfigMissing)?
        .clone();
    let extras = ex.get().map(|g| (*g).clone()).unwrap_or_default();
    let arc = P::new(cfg, extras).map_err(|e| ProviderCacheError::Build(e.into()))?;
    Ok(erase(arc))
}

fn drain_stream<S, T>(stream: &mut S) -> bool
where
    S: Stream<Item = T> + Unpin,
{
    let mut cx = Context::from_waker(noop_waker_ref());
    let mut fired = false;
    while let Poll::Ready(Some(_)) = stream.poll_next_unpin(&mut cx) {
        fired = true;
    }
    fired
}
