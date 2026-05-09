use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use frances_config::{ConfigBinding, ConfigHandle, RequiredConfigBinding};
use serde_json::Value;

use crate::history::{HistoryStore, OwnedHistoryInput};
use crate::llm::provider_cache::ProviderCache;
use frances_llm::{
    CompletionOutcome, ErasedError, HistoryInput, ModelConfig, ProviderRequest, StreamEvent,
    ToolChoice, ToolDef,
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

/// Chat-completions client. Owns the conversation's persistence: callers
/// `submit_user` / `submit_tool_result` to enqueue inputs, then `run` to
/// drive the LLM. History rows and primitive rows land in [`HistoryStore`]
/// automatically.
///
/// `complete` exists for one-shot, non-persisted calls (e.g. the shell
/// classifier) — it threads `history` and `new_inputs` through to the
/// provider verbatim and writes nothing.
#[derive(Clone)]
pub struct ChatClient {
    env: HashMap<OsString, OsString>,
    session_id: String,
    cache: Arc<ProviderCache>,
    config: ConfigHandle,
    default_model: RequiredConfigBinding<ModelConfig>,
    history: HistoryStore,
    model_cache: Arc<Mutex<HashMap<String, ConfigBinding<ModelConfig>>>>,
    /// Inputs accepted via `submit_user` / `submit_tool_result` since the
    /// last `run`. Drained by `run` and handed to the provider.
    pending: Arc<Mutex<Vec<OwnedHistoryInput>>>,
}

impl ChatClient {
    pub fn new(
        env: HashMap<OsString, OsString>,
        session_id: String,
        cache: Arc<ProviderCache>,
        config: ConfigHandle,
        default_model: RequiredConfigBinding<ModelConfig>,
        history: HistoryStore,
    ) -> Result<Self> {
        Ok(Self {
            env,
            session_id,
            cache,
            config,
            default_model,
            history,
            model_cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Persist a user-typed message and queue it for the next `run`.
    pub async fn submit_user(&self, text: &str) -> Result<()> {
        self.history.append_primitive_user(text).await?;
        self.pending
            .lock()
            .expect("chat pending poisoned")
            .push(OwnedHistoryInput::User {
                text: text.to_owned(),
            });
        Ok(())
    }

    /// Persist a tool result and queue it for the next `run`.
    pub async fn submit_tool_result(
        &self,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<()> {
        self.history
            .append_primitive_tool_result(call_id, content, is_error)
            .await?;
        self.pending
            .lock()
            .expect("chat pending poisoned")
            .push(OwnedHistoryInput::ToolResult {
                call_id: call_id.to_owned(),
                content: content.to_owned(),
                is_error,
            });
        Ok(())
    }

    /// Drive the LLM with the queued inputs + persisted history. Streams
    /// events to `on_event`; the daemon handles `TextDelta` / `ToolCall` /
    /// `Usage`. `StreamEvent::History` events are captured internally and
    /// persisted; they are *not* forwarded to `on_event`.
    pub async fn run<F>(
        &self,
        names: &[&str],
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
        let provider_kind = provider.kind();

        let pending: Vec<OwnedHistoryInput> =
            std::mem::take(&mut *self.pending.lock().expect("chat pending poisoned"));
        let new_inputs: Vec<HistoryInput<'_>> =
            pending.iter().map(OwnedHistoryInput::as_borrowed).collect();
        let history = self.history.loaded_history().await?;

        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            history: &history,
            new_inputs: &new_inputs,
            tools,
            tool_choice,
            env: &self.env,
        };

        let mut emitted_payloads: Vec<Value> = Vec::new();
        let mut wrapped = |ev: StreamEvent| match ev {
            StreamEvent::History(payload) => {
                emitted_payloads.push(payload);
                Ok(())
            }
            other => on_event(other).map_err(into_erased),
        };

        let completion = provider
            .stream(req, &mut wrapped)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))?;

        self.history
            .append_history(provider_kind, &provider_id, &emitted_payloads)
            .await?;

        if !completion.text.is_empty() {
            self.history
                .append_primitive_assistant(&completion.text)
                .await?;
        }
        for call in &completion.tool_calls {
            self.history
                .append_primitive_tool_call(&call.id, &call.name, &call.arguments)
                .await?;
        }

        Ok(completion)
    }

    /// One-shot, non-persisted call. Caller supplies the full `history` +
    /// `new_inputs`; nothing is read from or written to [`HistoryStore`].
    /// Used by the shell classifier.
    pub async fn complete(
        &self,
        names: &[&str],
        history: &[Value],
        new_inputs: &[HistoryInput<'_>],
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
            history,
            new_inputs,
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
    /// binding currently has a value. Falls through to `default_model`.
    fn resolve_model(&self, names: &[&str]) -> Result<ModelConfig> {
        for name in names {
            let binding = self.binding_for(name)?;
            if let Some(model) = binding.get() {
                return Ok((*model).clone());
            }
        }
        Ok((*self.default_model.get()).clone())
    }

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
