//! `Provider` trait + the type-erased boundary the cache stores. Value
//! types (`HistoryInput`, `StreamEvent`, `CompletionOutcome`, tool defs,
//! usage, etc.) live in `frances-models-llm`; this file is the
//! impl-facing machinery.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde_json::Value;

use frances_models_llm::config::{ModelConfig, ProviderConfig};
use frances_models_llm::wire::{
    ChunkAbort, CompletionOutcome, ErasedError, ErasedResult, HistoryInput, StreamEvent,
    ToolChoice, ToolDef,
};

/// All non-callback inputs to a single chat call, shared by `stream` and
/// `complete`. `session_id` is opaque to the provider — used only as a
/// cache-scoping hint (e.g. Anthropic prompt-cache breakpoints, OpenAI's
/// automatic cache key). Implementations that don't support token caching
/// ignore it.
///
/// `history` is the already-forged wire JSON from prior turns (whatever the
/// provider previously emitted as `StreamEvent::History`, in order).
/// `new_inputs` is the delta since last call — primitives the provider
/// should forge inline (emitting one `StreamEvent::History` per output) and
/// include in the request body.
pub struct ProviderRequest<'a> {
    pub session_id: &'a str,
    pub model: &'a ModelConfig,
    pub history: &'a [Value],
    pub new_inputs: &'a [HistoryInput<'a>],
    pub tools: &'a [ToolDef],
    pub tool_choice: Option<&'a ToolChoice>,
    pub env: &'a HashMap<OsString, OsString>,
}

/// Concrete provider trait. Each impl knows one wire (OpenAI chat
/// completions today). The cache wraps each impl in `ErasedProvider`
/// before storage.
#[async_trait]
pub trait Provider: Send + Sync {
    type Extras: DeserializeOwned + Default + Clone + Send + Sync + 'static;
    type BuildError: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;
    type Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;

    /// Stable identifier for this provider's wire shape. Tagged onto every
    /// persisted history row so we know what wire it was forged for.
    /// Conventionally each impl returns a `'static` literal.
    fn kind(&self) -> &'static str;

    fn new(
        provider_config: ProviderConfig,
        extras: Self::Extras,
    ) -> Result<Arc<Self>, Self::BuildError>
    where
        Self: Sized;

    /// Batch wire-encoder. Used **only** at swap time, when the daemon
    /// rebuilds the entire history cache from primitive rows under a new
    /// provider's tag. The output `Vec` may be a different length than
    /// `inputs` — a provider can map one primitive to several wire
    /// messages or coalesce many into one. Order is preserved.
    fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value>;

    /// Drive a single chat call. Typed `StreamEvent`s are delivered to
    /// `on_event` synchronously as they're parsed; the call's full result
    /// (concatenated text + finalised tool calls) is returned at the end.
    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), Self::Error> + Send),
    ) -> Result<CompletionOutcome, Self::Error>;

    /// Convenience: same as `stream` with a no-op event handler. Override
    /// only if you can be cheaper than the default.
    async fn complete(&self, req: ProviderRequest<'_>) -> Result<CompletionOutcome, Self::Error> {
        self.stream(req, &mut |_| Ok(())).await
    }
}

/// Sealed type-erased view over any `Provider`. The cache stores these.
#[derive(Clone)]
pub struct ErasedProvider {
    inner: Arc<dyn ErasedInner>,
}

trait ErasedInner: Send + Sync {
    fn kind(&self) -> &'static str;

    fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value>;

    fn stream<'a>(
        &'a self,
        req: ProviderRequest<'a>,
        on_event: &'a mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>>;

    fn complete<'a>(
        &'a self,
        req: ProviderRequest<'a>,
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>>;
}

impl<P> ErasedInner for P
where
    P: Provider + 'static,
    P::Error: Into<ErasedError> + From<ErasedError>,
{
    fn kind(&self) -> &'static str {
        Provider::kind(self)
    }

    fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value> {
        Provider::forge_history(self, inputs)
    }

    fn stream<'a>(
        &'a self,
        req: ProviderRequest<'a>,
        on_event: &'a mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>> {
        Box::pin(async move {
            let mut event_err: Option<ErasedError> = None;
            let mut wrapped = |ev: StreamEvent| -> std::result::Result<(), P::Error> {
                match on_event(ev) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let synthesised: P::Error = (Box::new(ChunkAbort) as ErasedError).into();
                        event_err = Some(e);
                        Err(synthesised)
                    }
                }
            };
            let res = self.stream(req, &mut wrapped).await;
            if let Some(e) = event_err {
                return Err(e);
            }
            res.map_err(P::Error::into)
        })
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest<'a>,
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>> {
        Box::pin(async move { self.complete(req).await.map_err(P::Error::into) })
    }
}

impl ErasedProvider {
    pub(crate) fn new<P>(provider: Arc<P>) -> Self
    where
        P: Provider + 'static,
        P::Error: Into<ErasedError> + From<ErasedError>,
    {
        Self { inner: provider }
    }

    pub fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    pub fn forge_history(&self, inputs: &[HistoryInput<'_>]) -> Vec<Value> {
        self.inner.forge_history(inputs)
    }

    pub async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> ErasedResult<CompletionOutcome> {
        self.inner.stream(req, on_event).await
    }

    pub async fn complete(&self, req: ProviderRequest<'_>) -> ErasedResult<CompletionOutcome> {
        self.inner.complete(req).await
    }
}

/// Internal helper for the cache: wrap a concrete `Arc<P>` in an
/// `Arc<ErasedProvider>` without leaking the `P::Error: Into<...>`
/// bound to call sites.
pub(crate) fn erase<P>(provider: Arc<P>) -> Arc<ErasedProvider>
where
    P: Provider + 'static,
    P::Error: Into<ErasedError> + From<ErasedError>,
{
    Arc::new(ErasedProvider::new(provider))
}
