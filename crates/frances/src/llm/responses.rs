use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use frances_config::{ConfigBinding, ConfigHandle, RequiredConfigBinding};
use serde_json::Value;

use crate::llm::provider_cache::ProviderCache;
use frances_llm::{
    CompletionOutcome, ErasedError, ModelConfig, ProviderRequest, StreamEvent, ToolChoice, ToolDef,
};

/// Convert an internal `anyhow::Error` to an [`ErasedError`] for the
/// [`frances_llm::ErasedProvider`] boundary.
fn into_erased(e: anyhow::Error) -> ErasedError {
    e.into()
}

/// Log the underlying erased error and substitute a generic message before
/// it crosses back into anyhow-using daemon code.
fn log_and_generic(provider_id: &str, e: ErasedError) -> anyhow::Error {
    tracing::error!(provider = %provider_id, error = %e, "provider error");
    anyhow!("provider {} encountered an error", provider_id)
}

/// Chat-completions client. Vendor-neutral despite the name "Responses"
/// in `wire_api`: today the wire is OpenAI-style chat completions; if a
/// new wire is introduced it will live in a sibling module.
///
/// Model selection is name-driven: callers pass a fallback list of names
/// (e.g. `&["chat"]`) and the client looks each one up under
/// `models::<name>`. Bindings are cached on first use so repeat lookups
/// are lock-free reads. The fallback always terminates in `default_model`,
/// which is bound as required at startup.
#[derive(Clone)]
pub struct ChatClient {
    env: HashMap<OsString, OsString>,
    session_id: String,
    cache: Arc<ProviderCache>,
    config: ConfigHandle,
    default_model: RequiredConfigBinding<ModelConfig>,
    model_cache: Arc<Mutex<HashMap<String, ConfigBinding<ModelConfig>>>>,
}

impl ChatClient {
    pub fn new(
        env: HashMap<OsString, OsString>,
        session_id: String,
        cache: Arc<ProviderCache>,
        config: ConfigHandle,
        default_model: RequiredConfigBinding<ModelConfig>,
    ) -> Result<Self> {
        Ok(Self {
            env,
            session_id,
            cache,
            config,
            default_model,
            model_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn stream<F>(
        &self,
        names: &[&str],
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
        mut on_event: F,
    ) -> Result<CompletionOutcome>
    where
        F: FnMut(StreamEvent) -> Result<()> + Send,
    {
        let model = self.resolve_model(names)?;
        let provider_id = model.model_provider.clone();
        let provider = self.cache.get(&provider_id).ok_or_else(|| {
            anyhow!(
                "model_providers.{} not available (no config or factory missing)",
                provider_id
            )
        })?;
        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            messages,
            tools,
            tool_choice,
            env: &self.env,
        };
        let mut wrapped = |ev: StreamEvent| on_event(ev).map_err(into_erased);
        provider
            .stream(req, &mut wrapped)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))
    }

    /// One-shot wrapper around [`stream`](Self::stream): drives the SSE
    /// stream to completion and returns the full assistant text plus any
    /// finalized tool calls. Use this when the caller doesn't need to
    /// surface mid-stream deltas (e.g. the shell classifier).
    pub async fn complete(
        &self,
        names: &[&str],
        messages: &[Value],
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
    ) -> Result<CompletionOutcome> {
        let model = self.resolve_model(names)?;
        let provider_id = model.model_provider.clone();
        let provider = self.cache.get(&provider_id).ok_or_else(|| {
            anyhow!(
                "model_providers.{} not available (no config or factory missing)",
                provider_id
            )
        })?;
        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            messages,
            tools,
            tool_choice,
            env: &self.env,
        };
        provider
            .complete(req)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))
    }

    /// Walks `names` in order, returning the first one whose `models::<name>`
    /// binding currently has a value. Falls through to `default_model`, which
    /// is a required (sticky-on-absence) binding and therefore always
    /// resolves. Bindings are looked up via [`Self::binding_for`] so the
    /// arc-swap snapshot is always consulted live and config updates are
    /// picked up without restart.
    fn resolve_model(&self, names: &[&str]) -> Result<ModelConfig> {
        for name in names {
            let binding = self.binding_for(name)?;
            if let Some(model) = binding.get() {
                return Ok((*model).clone());
            }
        }
        Ok((*self.default_model.get()).clone())
    }

    /// Returns the cached binding for `models::<name>`, creating one on first
    /// use. The cache stores the binding object, not its current value —
    /// `ConfigBinding::get()` is re-evaluated on every call so live config
    /// edits propagate. Names that are never present in config still get a
    /// binding kept around, which is fine: the set of distinct names asked
    /// for is bounded by the call sites in the binary.
    fn binding_for(&self, name: &str) -> Result<ConfigBinding<ModelConfig>> {
        let mut cache = self.model_cache.lock().expect("model_cache poisoned");
        if let Some(b) = cache.get(name) {
            return Ok(b.clone());
        }
        let b = self
            .config
            .bind::<ModelConfig>(["models", name])
            .with_context(|| format!("bind models::{name}"))?;
        cache.insert(name.to_string(), b.clone());
        Ok(b)
    }
}
