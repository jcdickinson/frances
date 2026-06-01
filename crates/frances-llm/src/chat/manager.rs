use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use frances_config::{ConfigBinding, ConfigHandle, RequiredConfigBinding};
use frances_models_llm::chat::{
    ChatError, ChatSessionBuilder, ChatSessionId, ChatSessionManager as ChatSessionManagerTrait,
    ChatSessionRow, CompleteRequest,
};
use frances_models_llm::config::ModelConfig;
use frances_models_llm::{CompletionOutcome, ErasedError, StreamEvent};
use serde::Deserialize;
use tracing::error;

use crate::chat::deps::ChatManagerDeps;
use crate::chat::session::ChatSession;
use crate::chat::store::HistoryStore;
use crate::provider::ProviderRequest;
use crate::provider_cache::ProviderCache;

/// Live snapshot of the entire `models::*` table. Bound once on the
/// manager; refreshes on any add/remove/change under that path. Sessions
/// look up by intent on every resolve.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Models(pub(crate) HashMap<Arc<str>, ModelConfig>);

/// Concrete chat-session manager. Clone-by-value handle; inner state
/// lives in `Arc<Inner>`.
pub struct ChatSessionManager<D: ChatManagerDeps> {
    pub(crate) inner: Arc<ChatSessionManagerInner<D>>,
}

impl<D: ChatManagerDeps> Clone for ChatSessionManager<D> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct ChatSessionManagerInner<D: ChatManagerDeps> {
    pub(crate) deps: D,
    pub(crate) default_model: RequiredConfigBinding<ModelConfig>,
    pub(crate) models: ConfigBinding<Models>,
    pub(crate) cache: ProviderCache,
}

impl<D: ChatManagerDeps> ChatSessionManager<D> {
    pub fn new(
        deps: D,
        config: ConfigHandle,
        default_model: RequiredConfigBinding<ModelConfig>,
        cache: ProviderCache,
    ) -> Result<Self, frances_config::ConfigBindError> {
        let models = config.bind::<Models>("models")?;
        Ok(Self {
            inner: Arc::new(ChatSessionManagerInner {
                deps,
                default_model,
                models,
                cache,
            }),
        })
    }

    pub fn deps(&self) -> &D {
        &self.inner.deps
    }

    pub(crate) fn cache(&self) -> &ProviderCache {
        &self.inner.cache
    }

    fn default_model(&self) -> ModelConfig {
        (*self.inner.default_model.get()).clone()
    }

    /// Walks `intents` and returns the first name that resolves to a
    /// model in the live snapshot. Falls through to `"default"` when
    /// none match. The returned `Arc<str>` is the actual map key (or a
    /// fresh `Arc::from("default")` for the fallback).
    pub(crate) fn resolve_name<S: AsRef<str>>(&self, intents: &[S]) -> Arc<str> {
        if let Some(models) = self.inner.models.get() {
            for n in intents {
                if let Some((k, _)) = models.0.get_key_value(n.as_ref()) {
                    return k.clone();
                }
            }
        }
        Arc::from("default")
    }

    /// Read the `ModelConfig` for a resolved name, with the
    /// `RequiredConfigBinding<ModelConfig>` at `models::default` as the
    /// fallback if the live `models` snapshot doesn't carry the entry.
    pub(crate) fn model_for(&self, name: &str) -> ModelConfig {
        self.inner
            .models
            .get()
            .and_then(|g| g.0.get(name).cloned())
            .unwrap_or_else(|| self.default_model())
    }

    /// Same as the trait's `complete`, but the caller observes
    /// every `StreamEvent` the provider emits.
    pub async fn complete_with_events(
        &self,
        req: CompleteRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<CompletionOutcome, ChatError> {
        let model_name = self.resolve_name(req.intents);
        let model = self.model_for(&model_name);
        let provider_id = model.model_provider.clone();
        let provider = self
            .inner
            .cache
            .get(&provider_id)
            .ok_or_else(|| ChatError::ProviderUnavailable(provider_id.clone()))?;
        let cancel = req.cancel.clone();
        let provider_req = ProviderRequest {
            session_id: req.session_id,
            model_name: &model_name,
            model: &model,
            history: req.history,
            new_inputs: req.new_inputs,
            tools: req.tools,
            tool_choice: req.tool_choice,
            env: req.env,
            max_tool_calls: req.max_tool_calls,
        };
        let mut wrapped = |ev: StreamEvent| -> Result<(), ErasedError> {
            on_event(ev);
            Ok(())
        };
        match provider
            .stream(provider_req, cancel.clone(), &mut wrapped)
            .await
        {
            Ok(mut c) => {
                frances_models_llm::tool_args::annotate(&mut c.tool_calls, req.tools);
                Ok(c)
            }
            Err(_) if cancel.is_cancelled() => Err(ChatError::Cancelled),
            Err(source) => Err(log_and_typed(&provider_id, source)),
        }
    }
}

#[async_trait]
impl<D: ChatManagerDeps> ChatSessionManagerTrait for ChatSessionManager<D> {
    type Session = ChatSession<D>;

    /// One-shot, non-persisted call. Resolves a model by walking
    /// `req.intents`, then calls the provider with `req.history` +
    /// `req.new_inputs` verbatim. Nothing is read from or written to
    /// the history store.
    async fn complete(&self, req: CompleteRequest<'_>) -> Result<CompletionOutcome, ChatError> {
        self.complete_with_events(req, &mut |_| {}).await
    }

    fn create(&self, builder: ChatSessionBuilder) -> Self::Session {
        let session_id = uuid::Uuid::new_v4().to_string();
        ChatSession::new(
            None,
            session_id,
            builder.model_intents,
            builder.ephemeral,
            self.clone(),
        )
    }

    async fn load(&self, id: ChatSessionId) -> Result<Self::Session, ChatError> {
        let ChatSessionRow {
            id,
            session_id,
            model_intents,
        } = self
            .inner
            .deps
            .history_store()
            .load_chat_session(id)
            .await?;
        // A `load`ed session was once persisted by definition; it can
        // never be ephemeral.
        Ok(ChatSession::new(
            Some(id),
            session_id,
            model_intents,
            false,
            self.clone(),
        ))
    }
}

pub(crate) fn log_and_typed(provider_id: &str, source: ErasedError) -> ChatError {
    error!(provider = %provider_id, error = %source, "provider error");
    ChatError::Provider {
        provider_id: provider_id.to_owned(),
        source,
    }
}
