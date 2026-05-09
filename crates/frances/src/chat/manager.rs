use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use frances_config::{ConfigBinding, ConfigHandle, RequiredConfigBinding};
use frances_llm::{
    CompletionOutcome, ErasedError, HistoryInput, ModelConfig, ProviderRequest, ToolChoice, ToolDef,
};
use serde::Deserialize;
use serde_json::Value;

use crate::chat::builder::ChatSessionBuilder;
use crate::chat::session::ChatSession;
use crate::history::{ChatSessionId, ChatSessionRow, HistoryStore};
use crate::llm::provider_cache::ProviderCache;

/// Live snapshot of the entire `models::*` table. Bound once on the
/// manager; refreshes on any add/remove/change under that path. Sessions
/// look up by intent on every resolve.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
struct Models(HashMap<String, ModelConfig>);

/// Inputs to [`ChatSessionManager::complete`]. Bundled so the call site
/// reads as `chat.complete(CompleteRequest { … })` instead of a wall of
/// positional args.
pub struct CompleteRequest<'a> {
    /// Model-intent names to walk; first hit wins, default fallback.
    pub intents: &'a [&'a str],
    /// Token-cache scope id. For classifier-style calls during a chat
    /// turn, pass the parent chat session's id.
    pub session_id: &'a str,
    pub env: &'a HashMap<OsString, OsString>,
    pub history: &'a [Value],
    pub new_inputs: &'a [HistoryInput<'a>],
    pub tools: &'a [ToolDef],
    pub tool_choice: Option<&'a ToolChoice>,
}

/// Shared per-daemon state behind every `ChatSession`. Sessions clone an
/// `Arc<Self>` to reach the cache, model snapshot, and history store.
pub struct ChatSessionManager {
    cache: Arc<ProviderCache>,
    default_model: RequiredConfigBinding<ModelConfig>,
    models: ConfigBinding<Models>,
    history: HistoryStore,
}

impl ChatSessionManager {
    pub fn new(
        cache: Arc<ProviderCache>,
        config: ConfigHandle,
        default_model: RequiredConfigBinding<ModelConfig>,
        history: HistoryStore,
    ) -> Result<Arc<Self>> {
        let models = config.bind::<Models>("models").context("bind models")?;
        Ok(Arc::new(Self {
            cache,
            default_model,
            models,
            history,
        }))
    }

    /// Mint a UUID, persist a `chat_sessions` row, and return a fresh
    /// `ChatSession` with the builder's `model_intents` baked in.
    pub async fn create(self: &Arc<Self>, builder: ChatSessionBuilder) -> Result<Arc<ChatSession>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let row_id = self
            .history
            .create_chat_session(&session_id, &builder.model_intents)
            .await?;
        Ok(Arc::new(ChatSession::new(
            row_id,
            session_id,
            builder.model_intents,
            self.clone(),
        )))
    }

    /// Load the persisted row and return a fresh `ChatSession`.
    pub async fn load(self: &Arc<Self>, id: ChatSessionId) -> Result<Arc<ChatSession>> {
        let ChatSessionRow {
            id,
            session_id,
            model_intents,
        } = self.history.load_chat_session(id).await?;
        Ok(Arc::new(ChatSession::new(
            id,
            session_id,
            model_intents,
            self.clone(),
        )))
    }

    /// Returns the pinned primary chat session, creating it from
    /// `builder` if no row exists yet. There's no way to start a fresh
    /// primary chat from the UI today, so this is effectively
    /// load-or-init on first daemon startup.
    pub async fn primary(
        self: &Arc<Self>,
        builder: ChatSessionBuilder,
    ) -> Result<Arc<ChatSession>> {
        if let Some(id) = self.history.primary_chat_session().await? {
            return self.load(id).await;
        }
        let session = self.create(builder).await?;
        self.history
            .insert_primary_chat_session(session.id())
            .await?;
        Ok(session)
    }

    /// One-shot, non-persisted call. Resolves a model by walking
    /// `req.intents`, then calls the provider with `req.history` +
    /// `req.new_inputs` verbatim. Nothing is read from or written to
    /// [`HistoryStore`]. Used by tools that need an LLM but aren't part
    /// of a persistent conversation (e.g. the shell classifier).
    pub async fn complete(&self, req: CompleteRequest<'_>) -> Result<CompletionOutcome> {
        let model = self.resolve_model(req.intents);
        let provider_id = model.model_provider.clone();
        let provider = self.cache.get(&provider_id).ok_or_else(|| {
            anyhow!(
                "model_providers.{} not available (no config or factory missing)",
                provider_id
            )
        })?;
        let provider_req = ProviderRequest {
            session_id: req.session_id,
            model: &model,
            history: req.history,
            new_inputs: req.new_inputs,
            tools: req.tools,
            tool_choice: req.tool_choice,
            env: req.env,
        };
        provider
            .complete(provider_req)
            .await
            .map_err(|e| log_and_generic(&provider_id, e))
    }

    pub(crate) fn cache(&self) -> &Arc<ProviderCache> {
        &self.cache
    }

    pub(crate) fn history(&self) -> &HistoryStore {
        &self.history
    }

    pub(crate) fn default_model(&self) -> ModelConfig {
        (*self.default_model.get()).clone()
    }

    /// Snapshot lookup of `models::<name>`. Returns the current value of
    /// the live binding; `None` if the model isn't currently defined.
    pub(crate) fn model(&self, name: &str) -> Option<ModelConfig> {
        self.models.get().and_then(|g| g.0.get(name).cloned())
    }

    /// Walks `intents` in order, returning the first one that resolves.
    /// Falls through to `default_model`.
    pub(crate) fn resolve_model<S: AsRef<str>>(&self, intents: &[S]) -> ModelConfig {
        for name in intents {
            if let Some(m) = self.model(name.as_ref()) {
                return m;
            }
        }
        self.default_model()
    }
}

fn log_and_generic(provider_id: &str, e: ErasedError) -> anyhow::Error {
    tracing::error!(provider = %provider_id, error = %e, "provider error");
    anyhow!("provider {} encountered an error", provider_id)
}
