use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use frances_llm::{
    CompletionOutcome, ErasedError, HistoryInput, ProviderRequest, StreamEvent, ToolChoice, ToolDef,
};
use serde_json::Value;

use crate::chat::manager::ChatSessionManager;
use crate::history::{ChatSessionId, OwnedHistoryInput};

fn into_erased(e: anyhow::Error) -> ErasedError {
    e.into()
}

fn log_and_generic(provider_id: &str, e: ErasedError) -> anyhow::Error {
    tracing::error!(provider = %provider_id, error = %e, "provider error");
    anyhow!("provider {} encountered an error", provider_id)
}

pub struct ChatSession {
    id: ChatSessionId,
    /// Opaque per-session UUID; threaded into `ProviderRequest::session_id`
    /// so token caching scopes to this chat alone.
    session_id: String,
    /// Ordered list of `models::<intent>` config keys to walk when
    /// resolving a model. Snapshot at session creation; the implicit
    /// `models::default` (a required binding) is the always-on final
    /// fallback.
    model_intents: Vec<String>,
    manager: Arc<ChatSessionManager>,
    /// Inputs queued via `submit_user` / `submit_tool_result` since the
    /// last `run`. Drained by `run`.
    pending: Mutex<Vec<OwnedHistoryInput>>,
}

impl ChatSession {
    pub(crate) fn new(
        id: ChatSessionId,
        session_id: String,
        model_intents: Vec<String>,
        manager: Arc<ChatSessionManager>,
    ) -> Self {
        Self {
            id,
            session_id,
            model_intents,
            manager,
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn id(&self) -> ChatSessionId {
        self.id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[cfg(test)]
    pub fn model_intents(&self) -> &[String] {
        &self.model_intents
    }

    pub async fn submit_user(&self, text: &str) -> Result<()> {
        self.manager
            .history()
            .append_primitive_user(self.id, text)
            .await?;
        self.pending
            .lock()
            .expect("chat pending poisoned")
            .push(OwnedHistoryInput::User {
                text: text.to_owned(),
            });
        Ok(())
    }

    pub async fn submit_tool_result(
        &self,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<()> {
        self.manager
            .history()
            .append_primitive_tool_result(self.id, call_id, content, is_error)
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

    pub async fn run<F>(
        &self,
        env: &HashMap<OsString, OsString>,
        tools: &[ToolDef],
        tool_choice: Option<&ToolChoice>,
        mut on_event: F,
    ) -> Result<CompletionOutcome>
    where
        F: FnMut(StreamEvent) -> Result<()> + Send,
    {
        let model = self.manager.resolve_model(&self.model_intents);
        let provider_id = model.model_provider.clone();
        let provider = self.manager.cache().get(&provider_id).ok_or_else(|| {
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
        let history = self.manager.history().loaded_history(self.id).await?;

        let req = ProviderRequest {
            session_id: &self.session_id,
            model: &model,
            history: &history,
            new_inputs: &new_inputs,
            tools,
            tool_choice,
            env,
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

        self.manager
            .history()
            .append_history(self.id, provider_kind, &provider_id, &emitted_payloads)
            .await?;

        if !completion.text.is_empty() {
            self.manager
                .history()
                .append_primitive_assistant(self.id, &completion.text)
                .await?;
        }
        for call in &completion.tool_calls {
            self.manager
                .history()
                .append_primitive_tool_call(self.id, &call.id, &call.name, &call.arguments)
                .await?;
        }

        Ok(completion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatSessionBuilder, ChatSessionManager};
    use crate::history::HistoryStore;
    use crate::llm::provider_cache::ProviderCache;
    use crate::store::test_support::TempDb;
    use frances_config::{
        ConfigEvent, ConfigHandle, ConfigProvider, EventSender, Path as CPath, ProviderError,
        Value as CValue,
    };
    use frances_llm::ModelConfig;
    use std::sync::Arc as StdArc;

    /// Test ConfigProvider that emits a fixed set of events on load.
    struct EagerProvider {
        events: std::sync::Mutex<Vec<ConfigEvent>>,
    }
    impl EagerProvider {
        fn new(events: Vec<ConfigEvent>) -> StdArc<Self> {
            StdArc::new(Self {
                events: std::sync::Mutex::new(events),
            })
        }
    }
    #[async_trait::async_trait]
    impl ConfigProvider for EagerProvider {
        async fn load(&self, sender: EventSender) -> std::result::Result<(), ProviderError> {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            if !events.is_empty() {
                sender.send(events).await.unwrap();
            }
            Ok(())
        }
    }

    fn ev(path: &str, value: impl Into<CValue>) -> ConfigEvent {
        ConfigEvent::new(CPath::parse(path), value)
    }

    async fn make_handle() -> ConfigHandle {
        let events = vec![
            // models::default — the always-on safety net.
            ev("models::default::model_provider", "openai"),
            ev("models::default::id", "gpt-4o-mini"),
            ev("models::default::max_tokens", CValue::Int(1000)),
            ev(
                "models::default::stream_idle_timeout_ms",
                CValue::Int(120_000),
            ),
            // models::deep — referenced by some tests' model_intents.
            ev("models::deep::model_provider", "openai"),
            ev("models::deep::id", "gpt-4o"),
            ev("models::deep::max_tokens", CValue::Int(2000)),
            ev("models::deep::stream_idle_timeout_ms", CValue::Int(180_000)),
        ];
        let provider = EagerProvider::new(events);
        ConfigHandle::build(vec![provider as StdArc<dyn ConfigProvider>])
            .await
            .unwrap()
    }

    async fn make_manager() -> Arc<ChatSessionManager> {
        let temp_db = TempDb::open().await;
        let config = make_handle().await;
        let cache = StdArc::new(ProviderCache::new(config.clone()).unwrap());
        let default_model = config
            .bind::<ModelConfig>(["models", "default"])
            .unwrap()
            .required()
            .unwrap();
        let history = HistoryStore::new((*temp_db).clone());
        // Leak the TempDb so the connection stays open for the lifetime of
        // the test. Tests are short; the OS reaps everything.
        std::mem::forget(temp_db);
        ChatSessionManager::new(cache, config, default_model, history).unwrap()
    }

    fn deep_builder() -> ChatSessionBuilder {
        ChatSessionBuilder::new().with_model_intents(["deep".to_string()])
    }

    #[tokio::test]
    async fn create_persists_session_row_and_returns_uuid() {
        let mgr = make_manager().await;
        let session = mgr.create(deep_builder()).await.unwrap();
        assert_eq!(session.model_intents(), ["deep"]);
        assert!(!session.session_id().is_empty());
        let id = session.id();
        let loaded = mgr.load(id).await.unwrap();
        assert_eq!(loaded.id(), id);
        assert_eq!(loaded.session_id(), session.session_id());
        assert_eq!(loaded.model_intents(), ["deep"]);
    }

    #[tokio::test]
    async fn submit_user_persists_primitive_scoped_to_session() {
        let mgr = make_manager().await;
        let a = mgr.create(deep_builder()).await.unwrap();
        let b = mgr.create(deep_builder()).await.unwrap();
        a.submit_user("hello from a").await.unwrap();
        b.submit_user("hello from b").await.unwrap();

        // Each session sees its own loaded_history, but neither has History
        // events yet — only primitives have been written.
        assert!(
            mgr.history()
                .loaded_history(a.id())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mgr.history()
                .loaded_history(b.id())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn submit_tool_result_persists_scoped_primitive() {
        let mgr = make_manager().await;
        let s = mgr.create(deep_builder()).await.unwrap();
        s.submit_tool_result("call_1", "ok", false).await.unwrap();
        assert!(
            mgr.history()
                .loaded_history(s.id())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn append_history_then_loaded_returns_payloads() {
        let mgr = make_manager().await;
        let s = mgr.create(deep_builder()).await.unwrap();
        let payloads = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        mgr.history()
            .append_history(s.id(), "test-wire", "openai", &payloads)
            .await
            .unwrap();
        assert_eq!(
            mgr.history().loaded_history(s.id()).await.unwrap(),
            payloads
        );
    }

    #[tokio::test]
    async fn history_does_not_leak_between_sessions() {
        let mgr = make_manager().await;
        let a = mgr.create(deep_builder()).await.unwrap();
        let b = mgr.create(deep_builder()).await.unwrap();
        mgr.history()
            .append_history(
                a.id(),
                "test-wire",
                "openai",
                &[serde_json::json!({"only": "in a"})],
            )
            .await
            .unwrap();
        assert_eq!(mgr.history().loaded_history(a.id()).await.unwrap().len(), 1);
        assert!(
            mgr.history()
                .loaded_history(b.id())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn resolve_model_walks_intents_then_default() {
        let mgr = make_manager().await;

        // Known intent → uses that one.
        assert_eq!(mgr.resolve_model(&["deep"]).id, "gpt-4o");

        // Unknown intent → falls through to default.
        assert_eq!(mgr.resolve_model(&["nope"]).id, "gpt-4o-mini");

        // No intents → falls through to default.
        assert_eq!(mgr.resolve_model::<&str>(&[]).id, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn primary_creates_then_reloads_same_session() {
        let mgr = make_manager().await;

        let first = mgr.primary(deep_builder()).await.unwrap();
        let again = mgr.primary(ChatSessionBuilder::new()).await.unwrap();

        // Second call must return the existing primary, not mint a new one
        // — even when the builder we hand it is different.
        assert_eq!(first.id(), again.id());
        assert_eq!(first.session_id(), again.session_id());
        assert_eq!(again.model_intents(), ["deep"]);
    }
}
