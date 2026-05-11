use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
use frances_models_llm::chat::{
    ChatError, ChatSession as ChatSessionTrait, ChatSessionId, HistoryError, OwnedHistoryInput,
};
use frances_models_llm::wire::{CompletionOutcome, ErasedError, StreamEvent, ToolChoice, ToolDef};
use parking_lot::Mutex;
use serde_json::Value;

use crate::chat::deps::ChatManagerDeps;
use crate::chat::manager::{ChatSessionManager, log_and_typed};
use crate::chat::store::HistoryStore;
use crate::provider::ProviderRequest;

fn into_erased(e: ChatError) -> ErasedError {
    Box::new(e)
}

/// Concrete chat session. Clone-by-value handle; complex state in
/// `Arc<Inner>`. Generic over the manager's deps `D` so the JS class
/// and tests can pin different impls.
pub struct ChatSession<D: ChatManagerDeps> {
    inner: Arc<ChatSessionInner<D>>,
}

impl<D: ChatManagerDeps> Clone for ChatSession<D> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct ChatSessionInner<D: ChatManagerDeps> {
    /// Set on first `run` via `ensure_row` (or up-front for `load`).
    id: Mutex<Option<ChatSessionId>>,
    /// Opaque per-session UUID; threaded into provider requests for
    /// token-cache scoping.
    session_id: String,
    /// Ordered list of `models::<intent>` config keys to walk when
    /// resolving a model. The implicit `models::default` (a required
    /// binding) is the always-on final fallback.
    model_intents: Vec<String>,
    manager: ChatSessionManager<D>,
    /// Inputs queued via `push` since the last `run`. Drained by `run`.
    pending: Mutex<Vec<OwnedHistoryInput>>,
}

impl<D: ChatManagerDeps> ChatSession<D> {
    pub(crate) fn new(
        id: Option<ChatSessionId>,
        session_id: String,
        model_intents: Vec<String>,
        manager: ChatSessionManager<D>,
    ) -> Self {
        Self {
            inner: Arc::new(ChatSessionInner {
                id: Mutex::new(id),
                session_id,
                model_intents,
                manager,
                pending: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn id(&self) -> Option<ChatSessionId> {
        *self.inner.id.lock()
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn model_intents(&self) -> &[String] {
        &self.inner.model_intents
    }

    /// TUI-compat shim. Behaviour: enqueue a user input for the next
    /// `run`. The primitive row is written by `run`, not here.
    pub async fn submit_user(&self, text: &str) -> Result<(), ChatError> {
        self.push_internal(OwnedHistoryInput::User {
            text: text.to_owned(),
        });
        Ok(())
    }

    /// TUI-compat shim. Same shape as `submit_user`.
    pub async fn submit_tool_result(
        &self,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<(), ChatError> {
        self.push_internal(OwnedHistoryInput::ToolResult {
            call_id: call_id.to_owned(),
            content: content.to_owned(),
            is_error,
        });
        Ok(())
    }

    fn push_internal(&self, input: OwnedHistoryInput) {
        self.inner.pending.lock().push(input);
    }

    /// Ensure the `chat_sessions` row exists. Idempotent. Used by the
    /// manager's `primary` and by `run` on first call.
    pub(crate) async fn ensure_row(&self) -> Result<ChatSessionId, HistoryError> {
        if let Some(id) = self.id() {
            return Ok(id);
        }
        let id = self
            .inner
            .manager
            .deps()
            .history_store()
            .create_chat_session(&self.inner.session_id, &self.inner.model_intents)
            .await?;
        *self.inner.id.lock() = Some(id);
        Ok(id)
    }
}

#[async_trait]
impl<D: ChatManagerDeps> ChatSessionTrait for ChatSession<D> {
    fn push(&self, input: OwnedHistoryInput) {
        self.push_internal(input);
    }

    async fn run(
        &self,
        env: HashMap<OsString, OsString>,
        tools: Vec<ToolDef>,
        tool_choice: Option<ToolChoice>,
        on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
    ) -> Result<CompletionOutcome, ChatError> {
        let mut on_event = on_event;
        let id = self.ensure_row().await?;
        let store = self.inner.manager.deps().history_store().clone();

        // Drain pending under the lock, then release it before any await.
        let drained: Vec<OwnedHistoryInput> = std::mem::take(&mut *self.inner.pending.lock());

        // Write primitives for drained entries first so the history
        // store is consistent before the network call.
        for input in &drained {
            store.append_primitive(id, input).await?;
        }

        let model = self.inner.manager.resolve_model(&self.inner.model_intents);
        let provider_id = model.model_provider.clone();
        let provider = self
            .inner
            .manager
            .cache()
            .get(&provider_id)
            .ok_or_else(|| ChatError::ProviderUnavailable(provider_id.clone()))?;
        let provider_kind = provider.kind();

        let new_inputs: Vec<_> = drained.iter().map(OwnedHistoryInput::as_borrowed).collect();
        let history = store.loaded_history(id).await?;

        let req = ProviderRequest {
            session_id: &self.inner.session_id,
            model: &model,
            history: &history,
            new_inputs: &new_inputs,
            tools: &tools,
            tool_choice: tool_choice.as_ref(),
            env: &env,
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
            .map_err(|source| log_and_typed(&provider_id, source))?;

        store
            .append_history(id, provider_kind, &provider_id, &emitted_payloads)
            .await?;

        if !completion.text.is_empty() {
            store
                .append_primitive_assistant(id, &completion.text)
                .await?;
        }
        for call in &completion.tool_calls {
            store
                .append_primitive_tool_call(id, &call.id, &call.name, &call.arguments)
                .await?;
        }

        Ok(completion)
    }
}
