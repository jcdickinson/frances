use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

use anyhow::{Result, anyhow};
use frances_config::{ConfigBinding, ConfigHandle, Keys};
use futures::Stream;
use futures::StreamExt;
use futures::task::noop_waker_ref;
use tracing::warn;

use crate::llm::config::{ProviderConfig, ResponsesModelExtras, WireApi};
use crate::llm::provider::{ErasedError, ErasedProvider, OpenAiLikeProvider, Provider};

/// Drain-on-get cache of constructed [`ErasedProvider`]s, keyed by provider
/// id. Each entry owns the subscribe streams for its `ProviderConfig` and
/// `Extras` bindings; on every [`get`](Self::get) we drain those streams
/// non-blockingly, and only rebuild when at least one yielded.
///
/// Construction of a per-id provider goes through a [`EntryFactory`] picked
/// by [`ProviderConfig::wire_api`]. Today only `WireApi::Responses →
/// OpenAiLikeProvider` is registered.
pub struct ProviderCache {
    handle: ConfigHandle,
    keys: ConfigBinding<Keys>,
    keys_stream: Mutex<KeysStream>,
    last_keys: Mutex<Keys>,
    entries: RwLock<HashMap<String, Mutex<Entry>>>,
    factories: HashMap<WireApi, Arc<dyn EntryFactory>>,
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

trait EntryFactory: Send + Sync {
    fn build(&self, handle: &ConfigHandle, id: &str) -> Result<Entry>;
}

impl ProviderCache {
    pub fn new(handle: ConfigHandle) -> Result<Self> {
        let keys = handle
            .bind::<Keys>("model_providers")
            .map_err(|e| anyhow!("bind model_providers: {e}"))?;
        // subscribe_now seeds the stream with the current snapshot so the
        // first `refresh_id_set` learns the existing key set.
        let keys_stream = keys.subscribe_now();
        let mut factories: HashMap<WireApi, Arc<dyn EntryFactory>> = HashMap::new();
        factories.insert(WireApi::Responses, Arc::new(OpenAiLikeFactory));
        Ok(Self {
            handle,
            keys,
            keys_stream: Mutex::new(keys_stream),
            last_keys: Mutex::new(Keys::default()),
            entries: RwLock::new(HashMap::new()),
            factories,
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<ErasedProvider>> {
        self.refresh_id_set();
        let id_lc = id.to_ascii_lowercase();
        {
            let entries = self.entries.read().expect("provider cache poisoned");
            if let Some(em) = entries.get(&id_lc) {
                return Some(em.lock().expect("provider entry poisoned").current());
            }
        }
        self.try_build(&id_lc)
    }

    fn refresh_id_set(&self) {
        let fired = {
            let mut s = self.keys_stream.lock().expect("keys stream poisoned");
            drain_stream(&mut *s)
        };
        if !fired {
            return;
        }
        let new_keys: Keys = self.keys.get().map(|g| (*g).clone()).unwrap_or_default();
        let mut last = self.last_keys.lock().expect("last_keys poisoned");
        let diff = new_keys.diff(&last);
        *last = new_keys;
        drop(last);
        if diff.removed.is_empty() {
            return;
        }
        let mut entries = self.entries.write().expect("provider cache poisoned");
        for id in &diff.removed {
            entries.remove(&id.to_ascii_lowercase());
        }
    }

    fn try_build(&self, id_lc: &str) -> Option<Arc<ErasedProvider>> {
        let pc = self
            .handle
            .bind::<ProviderConfig>(["model_providers", id_lc])
            .ok()?;
        let cfg = pc.get()?;
        let factory = self.factories.get(&cfg.wire_api)?.clone();
        drop(cfg);
        match factory.build(&self.handle, id_lc) {
            Ok(entry) => {
                let provider = entry.provider.clone();
                self.entries
                    .write()
                    .expect("provider cache poisoned")
                    .insert(id_lc.to_owned(), Mutex::new(entry));
                Some(provider)
            }
            Err(e) => {
                warn!(provider = %id_lc, error = %e, "provider factory failed");
                None
            }
        }
    }
}

struct OpenAiLikeFactory;

impl EntryFactory for OpenAiLikeFactory {
    fn build(&self, handle: &ConfigHandle, id: &str) -> Result<Entry> {
        let pc = handle
            .bind::<ProviderConfig>(["model_providers", id])
            .map_err(|e| anyhow!("bind model_providers::{id}: {e}"))?;
        let ex = handle
            .bind::<ResponsesModelExtras>(["model_provider_extensions", id])
            .map_err(|e| anyhow!("bind model_provider_extensions::{id}: {e}"))?;

        let initial = build_provider::<OpenAiLikeProvider>(&pc, &ex)?;

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
            match build_provider::<OpenAiLikeProvider>(&pc_for_refresh, &ex_for_refresh) {
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
}

fn build_provider<P>(
    pc: &ConfigBinding<ProviderConfig>,
    ex: &ConfigBinding<P::Extras>,
) -> Result<Arc<ErasedProvider>>
where
    P: Provider + 'static,
    P::Error: Into<ErasedError> + From<ErasedError>,
{
    let cfg = pc
        .get()
        .ok_or_else(|| anyhow!("ProviderConfig missing"))?
        .clone();
    let extras = ex.get().map(|g| (*g).clone()).unwrap_or_default();
    let arc = P::new(cfg, extras).map_err(|e| anyhow!("provider build: {e}"))?;
    Ok(Arc::new(ErasedProvider::new(arc)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::ProviderRequest;
    use async_trait::async_trait;
    use frances_config::{
        ConfigEvent, ConfigProvider, EventSender, Path, ProviderError, Value as CValue,
    };
    use serde::Deserialize;
    use std::time::Duration;
    use tokio::time::sleep;

    /// Test-only `Provider` impl that records its construction args. No HTTP.
    #[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
    struct TinyExtras {
        #[serde(default)]
        knob: Option<String>,
    }

    struct Tiny {
        #[allow(dead_code)]
        config: ProviderConfig,
        #[allow(dead_code)]
        extras: TinyExtras,
    }

    #[async_trait]
    impl Provider for Tiny {
        type Extras = TinyExtras;
        type BuildError = ErasedError;
        type Error = ErasedError;

        fn new(
            config: ProviderConfig,
            extras: Self::Extras,
        ) -> std::result::Result<Arc<Self>, ErasedError> {
            Ok(Arc::new(Tiny { config, extras }))
        }

        async fn stream(
            &self,
            _req: ProviderRequest<'_>,
            _on_chunk: &mut (
                     dyn for<'v> FnMut(
                &'v serde_json::Value,
            ) -> std::result::Result<(), ErasedError>
                         + Send
                 ),
        ) -> std::result::Result<(), ErasedError> {
            Ok(())
        }

        async fn complete(
            &self,
            _req: ProviderRequest<'_>,
        ) -> std::result::Result<crate::llm::responses::CompletionOutcome, ErasedError> {
            Ok(crate::llm::responses::CompletionOutcome {
                text: String::new(),
                tool_calls: Vec::new(),
            })
        }
    }

    /// Latching provider mirroring the one in `frances-config/tests/integration.rs`.
    struct LatchingProvider {
        initial: std::sync::Mutex<Vec<ConfigEvent>>,
        sender: std::sync::Mutex<Option<EventSender>>,
    }
    impl LatchingProvider {
        fn new(initial: Vec<ConfigEvent>) -> Arc<Self> {
            Arc::new(Self {
                initial: std::sync::Mutex::new(initial),
                sender: std::sync::Mutex::new(None),
            })
        }
        async fn emit(&self, events: Vec<ConfigEvent>) {
            let s = self
                .sender
                .lock()
                .unwrap()
                .clone()
                .expect("provider must have loaded");
            s.send(events).await.unwrap();
        }
    }
    #[async_trait]
    impl ConfigProvider for LatchingProvider {
        async fn load(&self, events: EventSender) -> std::result::Result<(), ProviderError> {
            let initial = std::mem::take(&mut *self.initial.lock().unwrap());
            if !initial.is_empty() {
                events.send(initial).await.unwrap();
            }
            *self.sender.lock().unwrap() = Some(events);
            Ok(())
        }
    }

    /// A `ProviderCache` wired with a `Tiny` factory only.
    fn tiny_cache(handle: ConfigHandle) -> Result<ProviderCache> {
        let mut cache = ProviderCache::new(handle)?;
        // Replace the default factory map with one that builds Tiny for any wire.
        struct TinyFactory;
        impl EntryFactory for TinyFactory {
            fn build(&self, handle: &ConfigHandle, id: &str) -> Result<Entry> {
                let pc = handle
                    .bind::<ProviderConfig>(["model_providers", id])
                    .map_err(|e| anyhow!("{e}"))?;
                let ex = handle
                    .bind::<TinyExtras>(["model_provider_extensions", id])
                    .map_err(|e| anyhow!("{e}"))?;
                let initial = build_provider::<Tiny>(&pc, &ex)?;
                let mut pc_stream = pc.subscribe();
                let mut ex_stream = ex.subscribe();
                let pc2 = pc.clone();
                let ex2 = ex.clone();
                let refresh: RefreshFn = Box::new(move || {
                    let any = drain_stream(&mut pc_stream) | drain_stream(&mut ex_stream);
                    if !any {
                        return None;
                    }
                    build_provider::<Tiny>(&pc2, &ex2).ok()
                });
                Ok(Entry {
                    provider: initial,
                    refresh,
                })
            }
        }
        cache.factories.clear();
        cache
            .factories
            .insert(WireApi::Responses, Arc::new(TinyFactory));
        Ok(cache)
    }

    fn ev(path: &str, value: impl Into<CValue>) -> ConfigEvent {
        ConfigEvent::new(Path::parse(path), value)
    }

    fn cfg_event(id: &str) -> Vec<ConfigEvent> {
        vec![
            ev(
                &format!("model_providers::{id}::base_url"),
                "https://example.com/",
            ),
            ev(&format!("model_providers::{id}::auth::token"), "sk-test"),
        ]
    }

    #[tokio::test]
    async fn lazy_creation_and_no_op_get_returns_same_arc() {
        let manual = LatchingProvider::new(cfg_event("foo"));
        let providers: Vec<Arc<dyn ConfigProvider>> = vec![manual.clone()];
        let handle = ConfigHandle::build(providers).await.unwrap();
        let cache = tiny_cache(handle).unwrap();

        let a = cache.get("foo").expect("entry created");
        let b = cache.get("foo").expect("entry exists");
        assert!(Arc::ptr_eq(&a, &b), "no events ⇒ no rebuild");
    }

    #[tokio::test]
    async fn extras_change_rebuilds_entry() {
        let manual = LatchingProvider::new(cfg_event("foo"));
        let providers: Vec<Arc<dyn ConfigProvider>> = vec![manual.clone()];
        let handle = ConfigHandle::build(providers).await.unwrap();
        let cache = tiny_cache(handle).unwrap();

        let a = cache.get("foo").expect("entry created");
        manual
            .emit(vec![ev("model_provider_extensions::foo::knob", "v1")])
            .await;
        sleep(Duration::from_millis(20)).await;
        let b = cache.get("foo").expect("still there");
        assert!(!Arc::ptr_eq(&a, &b), "extras changed ⇒ rebuild");
    }

    #[tokio::test]
    async fn id_removal_drops_entry() {
        let manual = LatchingProvider::new(cfg_event("foo"));
        let providers: Vec<Arc<dyn ConfigProvider>> = vec![manual.clone()];
        let handle = ConfigHandle::build(providers).await.unwrap();
        let cache = tiny_cache(handle).unwrap();

        assert!(cache.get("foo").is_some());
        manual
            .emit(vec![
                ConfigEvent::unset(Path::parse("model_providers::foo::base_url")),
                ConfigEvent::unset(Path::parse("model_providers::foo::auth::token")),
            ])
            .await;
        sleep(Duration::from_millis(20)).await;
        assert!(cache.get("foo").is_none());
    }
}
