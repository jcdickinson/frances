use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
use frances_models_llm::chat::{
    ChatCheckpoint, ChatError, ChatSession as ChatSessionTrait, ChatSessionId, HistoryBatch,
    HistoryError, ModelIntents, OwnedHistoryInput,
};
use frances_models_llm::{CompletionOutcome, ErasedError, StreamEvent, ToolChoice, ToolDef};
use parking_lot::Mutex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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
    /// Stays `None` forever for `ephemeral` sessions.
    id: Mutex<Option<ChatSessionId>>,
    /// Opaque per-session UUID; threaded into provider requests for
    /// token-cache scoping.
    session_id: String,
    /// Ordered list of `models::<intent>` config keys to walk when
    /// resolving a model. The implicit `models::default` (a required
    /// binding) is the always-on final fallback.
    model_intents: ModelIntents,
    /// When `true`, `run` skips every `HistoryStore` call and the
    /// provider sees only the in-memory `pending` drain.
    ephemeral: bool,
    manager: ChatSessionManager<D>,
    /// Inputs queued via `push` since the last `run`. Drained by `run`.
    pending: Mutex<Vec<OwnedHistoryInput>>,
}

impl<D: ChatManagerDeps> ChatSession<D> {
    pub(crate) fn new(
        id: Option<ChatSessionId>,
        session_id: String,
        model_intents: ModelIntents,
        ephemeral: bool,
        manager: ChatSessionManager<D>,
    ) -> Self {
        Self {
            inner: Arc::new(ChatSessionInner {
                id: Mutex::new(id),
                session_id,
                model_intents,
                ephemeral,
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

    pub fn model_intents(&self) -> &[std::borrow::Cow<'static, str>] {
        &self.inner.model_intents
    }

    pub fn is_ephemeral(&self) -> bool {
        self.inner.ephemeral
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

    /// Ensure the `chat_sessions` row exists. Idempotent. Used by
    /// `run` on the first call.
    ///
    /// Ephemeral sessions return `Ok(None)` without touching the
    /// history store; their `id` slot stays `None` for the session's
    /// entire lifetime.
    pub(crate) async fn ensure_row(&self) -> Result<Option<ChatSessionId>, HistoryError> {
        if self.inner.ephemeral {
            return Ok(None);
        }
        if let Some(id) = self.id() {
            return Ok(Some(id));
        }
        let id = self
            .inner
            .manager
            .deps()
            .history_store()
            .create_chat_session(&self.inner.session_id, &self.inner.model_intents)
            .await?;
        *self.inner.id.lock() = Some(id);
        Ok(Some(id))
    }
}

#[async_trait]
impl<D: ChatManagerDeps> ChatSessionTrait for ChatSession<D> {
    fn push(&self, input: OwnedHistoryInput) {
        self.push_internal(input);
    }

    async fn run(
        &self,
        env: Arc<HashMap<OsString, OsString>>,
        tools: Vec<ToolDef>,
        tool_choice: Option<ToolChoice>,
        cancel: CancellationToken,
        max_tool_calls: Option<usize>,
        on_event: Box<dyn FnMut(StreamEvent) -> Result<(), ChatError> + Send>,
    ) -> Result<CompletionOutcome, ChatError> {
        let mut on_event = on_event;
        // `None` for ephemeral sessions; otherwise the row id for this chat.
        let id = self.ensure_row().await?;
        let store = self.inner.manager.deps().history_store().clone();

        // Drain pending under the lock, then release it before any await.
        let drained: Vec<OwnedHistoryInput> = std::mem::take(&mut *self.inner.pending.lock());

        // Write primitives for drained entries first so the history
        // store is consistent before the network call. Skipped for
        // ephemeral sessions.
        if let Some(id) = id {
            let mut batch = HistoryBatch::default();
            for input in &drained {
                batch.primitive(input)?;
            }
            store.flush(id, batch).await?;
        }

        let model_name = self.inner.manager.resolve_name(&self.inner.model_intents);
        let model = self.inner.manager.model_for(&model_name);
        let provider_id = model.model_provider.clone();
        let provider = self
            .inner
            .manager
            .cache()
            .get(&provider_id)
            .ok_or_else(|| ChatError::ProviderUnavailable(provider_id.clone()))?;
        let provider_kind = provider.kind();

        let new_inputs: Vec<_> = drained.iter().map(OwnedHistoryInput::as_borrowed).collect();
        // Ephemeral sessions have no persisted history — the provider
        // sees only `new_inputs` (the in-memory drain).
        let history = match id {
            Some(id) => store.loaded_history(id).await?,
            None => Vec::new(),
        };

        let req = ProviderRequest {
            session_id: &self.inner.session_id,
            model_name: &model_name,
            model: &model,
            history: &history,
            new_inputs: &new_inputs,
            tools: &tools,
            tool_choice: tool_choice.as_ref(),
            env: env.as_ref(),
            max_tool_calls,
        };

        let mut emitted_payloads: Vec<Value> = Vec::new();
        let mut wrapped = |ev: StreamEvent| match ev {
            StreamEvent::History(payload) => {
                emitted_payloads.push(payload);
                Ok(())
            }
            other => on_event(other).map_err(into_erased),
        };

        let mut completion = match provider.stream(req, cancel.clone(), &mut wrapped).await {
            Ok(c) => c,
            Err(_) if cancel.is_cancelled() => return Err(ChatError::Cancelled),
            Err(source) => return Err(log_and_typed(&provider_id, source)),
        };
        frances_models_llm::tool_args::annotate(&mut completion.tool_calls, &tools);

        if let Some(id) = id {
            let mut batch = HistoryBatch::default();
            for payload in &emitted_payloads {
                batch.history(payload, provider_kind, &provider_id)?;
            }
            if !completion.text.is_empty() {
                batch.primitive(&OwnedHistoryInput::Assistant {
                    text: completion.text.clone(),
                })?;
            }
            for call in &completion.tool_calls {
                batch.primitive(&OwnedHistoryInput::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })?;
            }
            store.flush(id, batch).await?;
        }

        Ok(completion)
    }

    async fn checkpoint(&self) -> Result<ChatCheckpoint, ChatError> {
        let pending_len = self.inner.pending.lock().len();
        // Mint the row eagerly for non-ephemeral sessions so the marker
        // is valid even if the first `run` (which would otherwise mint
        // it) lands between this checkpoint and a rollback.
        let persisted = match self.ensure_row().await? {
            Some(id) => Some(
                self.inner
                    .manager
                    .deps()
                    .history_store()
                    .checkpoint(id)
                    .await?,
            ),
            None => None,
        };
        Ok(ChatCheckpoint {
            persisted,
            pending_len,
        })
    }

    async fn rollback(&self, checkpoint: ChatCheckpoint) -> Result<(), ChatError> {
        {
            let mut pending = self.inner.pending.lock();
            if checkpoint.pending_len < pending.len() {
                pending.truncate(checkpoint.pending_len);
            }
        }
        if let (Some(id), Some(to)) = (self.id(), checkpoint.persisted) {
            self.inner
                .manager
                .deps()
                .history_store()
                .rollback(id, to)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use frances_config::{ConfigHandle, ConfigProvider, InMemoryProvider};
    use frances_models_llm::chat::{
        BatchRow, ChatSession as ChatSessionTrait, ChatSessionBuilder, ChatSessionId,
        ChatSessionManager as ChatSessionManagerTrait, ChatSessionRow, HistoryBatch, HistoryError,
        OwnedHistoryInput,
    };
    use frances_models_llm::config::ModelConfig;
    use frances_models_llm::{CompletionOutcome, HistoryInput, StreamEvent, ToolCall};
    use serde_json::{Value, json};

    use crate::chat::deps::ChatManagerDeps;
    use crate::chat::manager::ChatSessionManager;
    use crate::chat::store::HistoryStore;
    use crate::provider_cache::ProviderCache;
    use crate::test_util::{StubProvider, StubScript};
    use frances_models_llm::chat::{CompleteRequest, Demand, EnforceError};

    /// `HistoryStore` impl that counts each method invocation. Lets the
    /// ephemeral test assert "zero writes" and the persisted test
    /// assert "correct number of writes".
    #[derive(Clone, Default)]
    struct CountingStore {
        next_id: Arc<AtomicI64>,
        create_chat_session: Arc<AtomicUsize>,
        loaded_history: Arc<AtomicUsize>,
        append_history: Arc<AtomicUsize>,
        append_primitive_user: Arc<AtomicUsize>,
        append_primitive_system: Arc<AtomicUsize>,
        append_primitive_assistant: Arc<AtomicUsize>,
        append_primitive_tool_call: Arc<AtomicUsize>,
        append_primitive_tool_result: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl HistoryStore for CountingStore {
        async fn create_chat_session(
            &self,
            _session_id: &str,
            _model_intents: &[Cow<'static, str>],
        ) -> Result<ChatSessionId, HistoryError> {
            self.create_chat_session.fetch_add(1, Ordering::Relaxed);
            Ok(ChatSessionId(self.next_id.fetch_add(1, Ordering::Relaxed)))
        }

        async fn load_chat_session(
            &self,
            _id: ChatSessionId,
        ) -> Result<ChatSessionRow, HistoryError> {
            unreachable!("load is not exercised in these tests")
        }

        async fn loaded_history(
            &self,
            _session: ChatSessionId,
        ) -> Result<Vec<Value>, HistoryError> {
            self.loaded_history.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }

        async fn append_history(
            &self,
            _session: ChatSessionId,
            _kind: &str,
            _provider_id: &str,
            _payloads: &[Value],
        ) -> Result<(), HistoryError> {
            self.append_history.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn flush(
            &self,
            _session: ChatSessionId,
            batch: HistoryBatch,
        ) -> Result<(), HistoryError> {
            for row in &batch.rows {
                let counter = match row {
                    BatchRow::History { .. } => &self.append_history,
                    BatchRow::Primitive { ty: "system", .. } => &self.append_primitive_system,
                    BatchRow::Primitive { ty: "user", .. } => &self.append_primitive_user,
                    BatchRow::Primitive {
                        ty: "assistant", ..
                    } => &self.append_primitive_assistant,
                    BatchRow::Primitive {
                        ty: "tool_call", ..
                    } => &self.append_primitive_tool_call,
                    BatchRow::Primitive { .. } => &self.append_primitive_tool_result,
                };
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestDeps {
        store: CountingStore,
    }

    impl ChatManagerDeps for TestDeps {
        type HistoryStore = CountingStore;
        fn history_store(&self) -> &Self::HistoryStore {
            &self.store
        }
    }

    /// Build everything a `ChatSession` needs: a real `ConfigHandle`
    /// pointing `models::default` at a "stub" provider, plus a
    /// `ProviderCache` with a `StubProvider` pre-inserted. Returns
    /// `(manager, counting-store, stub-provider)` for assertion access.
    async fn build_manager() -> (
        ChatSessionManager<TestDeps>,
        CountingStore,
        Arc<StubProvider>,
    ) {
        let provider: Arc<dyn ConfigProvider> = Arc::new(
            InMemoryProvider::new()
                .set("models::default::model_provider", "stub")
                .set("models::default::id", "stub-model"),
        );
        let handle = ConfigHandle::build(vec![provider]).await.unwrap();
        let default_model = handle
            .bind::<ModelConfig>(["models", "default"])
            .unwrap()
            .required()
            .unwrap();
        let cache = ProviderCache::new(handle.clone()).unwrap();
        let stub = Arc::new(StubProvider::new());
        cache.insert_stub("stub", stub.clone());
        let store = CountingStore::default();
        let manager = ChatSessionManager::new(
            TestDeps {
                store: store.clone(),
            },
            handle,
            default_model,
            cache,
        )
        .unwrap();
        (manager, store, stub)
    }

    fn assistant_script(text: &str) -> StubScript {
        StubScript {
            events: vec![StreamEvent::TextDelta(text.to_owned())],
            outcome: CompletionOutcome {
                text: text.to_owned(),
                tool_calls: Vec::new(),
            },
        }
    }

    /// Like [`assistant_script`] but also emits one forged-history payload,
    /// so the flush writes a `history` row (exercising that path).
    fn assistant_script_with_history(text: &str) -> StubScript {
        StubScript {
            events: vec![
                StreamEvent::History(json!({ "role": "assistant", "content": text })),
                StreamEvent::TextDelta(text.to_owned()),
            ],
            outcome: CompletionOutcome {
                text: text.to_owned(),
                tool_calls: Vec::new(),
            },
        }
    }

    fn tool_call_script(call_id: &str, name: &str, arguments: Value) -> StubScript {
        let call = ToolCall {
            error: None,
            id: call_id.to_owned(),
            name: name.to_owned(),
            arguments,
        };
        StubScript {
            events: vec![StreamEvent::ToolCall(call.clone())],
            outcome: CompletionOutcome {
                text: String::new(),
                tool_calls: vec![call],
            },
        }
    }

    async fn run_once<D: ChatManagerDeps>(session: &super::ChatSession<D>) {
        session
            .run(
                Arc::new(std::collections::HashMap::new()),
                Vec::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
                Box::new(|_| Ok(())),
            )
            .await
            .expect("run should succeed");
    }

    #[tokio::test]
    async fn ephemeral_session_writes_nothing_across_two_rounds() {
        let (manager, store, stub) = build_manager().await;
        stub.push_script(tool_call_script("call-1", "lookup", json!({"q": "x"})));
        stub.push_script(assistant_script("done"));

        let session = manager.create(ChatSessionBuilder::new().with_ephemeral(true));
        assert!(session.is_ephemeral());
        assert!(session.id().is_none());

        // Round 1: user push + provider returns a tool_call.
        session.push(OwnedHistoryInput::User {
            text: "round one".to_owned(),
        });
        run_once(&session).await;

        // Round 2: workflow would push the tool result, then user keeps going.
        session.push(OwnedHistoryInput::ToolResult {
            call_id: "call-1".to_owned(),
            content: "the-answer".to_owned(),
            is_error: false,
        });
        session.push(OwnedHistoryInput::User {
            text: "round two".to_owned(),
        });
        run_once(&session).await;

        // No DB activity at all.
        assert_eq!(store.create_chat_session.load(Ordering::Relaxed), 0);
        assert_eq!(store.loaded_history.load(Ordering::Relaxed), 0);
        assert_eq!(store.append_history.load(Ordering::Relaxed), 0);
        assert_eq!(store.append_primitive_user.load(Ordering::Relaxed), 0);
        assert_eq!(store.append_primitive_assistant.load(Ordering::Relaxed), 0);
        assert_eq!(store.append_primitive_tool_call.load(Ordering::Relaxed), 0);
        assert_eq!(
            store.append_primitive_tool_result.load(Ordering::Relaxed),
            0
        );
        assert!(session.id().is_none(), "ephemeral session never gets an id");

        // The provider sees only the in-memory drain. Round 2's
        // `history` is empty (no `loaded_history` substitution); the
        // tool result from round 1 rides in via `new_inputs`.
        let captured = stub.captured();
        assert_eq!(captured.len(), 2);
        assert!(captured[1].history.is_empty(), "ephemeral has no history");
        let r2_kinds: Vec<&str> = captured[1]
            .new_inputs
            .iter()
            .map(|i| match i {
                OwnedHistoryInput::User { .. } => "user",
                OwnedHistoryInput::ToolResult { .. } => "tool_result",
                _ => "other",
            })
            .collect();
        assert_eq!(
            r2_kinds,
            vec!["tool_result", "user"],
            "round 2 must include the tool result from round 1"
        );
    }

    #[tokio::test]
    async fn persisted_session_writes_each_primitive() {
        let (manager, store, stub) = build_manager().await;
        stub.push_script(assistant_script_with_history("hello"));
        stub.push_script(assistant_script_with_history("again"));

        let session = manager.create(ChatSessionBuilder::new());
        assert!(!session.is_ephemeral());

        session.push(OwnedHistoryInput::User {
            text: "round one".to_owned(),
        });
        run_once(&session).await;

        // After round 1: row created, user + assistant appended, history
        // payload appended, loaded_history queried once.
        assert_eq!(store.create_chat_session.load(Ordering::Relaxed), 1);
        assert_eq!(store.append_primitive_user.load(Ordering::Relaxed), 1);
        assert_eq!(store.append_primitive_assistant.load(Ordering::Relaxed), 1);
        assert_eq!(store.loaded_history.load(Ordering::Relaxed), 1);
        assert_eq!(store.append_history.load(Ordering::Relaxed), 1);
        assert!(session.id().is_some());

        session.push(OwnedHistoryInput::User {
            text: "round two".to_owned(),
        });
        run_once(&session).await;

        // `ensure_row` is idempotent — still one create.
        assert_eq!(store.create_chat_session.load(Ordering::Relaxed), 1);
        assert_eq!(store.append_primitive_user.load(Ordering::Relaxed), 2);
        assert_eq!(store.append_primitive_assistant.load(Ordering::Relaxed), 2);
        assert_eq!(store.loaded_history.load(Ordering::Relaxed), 2);
        assert_eq!(store.append_history.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cap_truncates_outcome_to_first_n_calls() {
        let (manager, _store, stub) = build_manager().await;
        // A script that wants to emit three tool calls.
        let three_calls = StubScript {
            events: vec![
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c1".into(),
                    name: "a".into(),
                    arguments: json!({"i": 1}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c2".into(),
                    name: "b".into(),
                    arguments: json!({"i": 2}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c3".into(),
                    name: "c".into(),
                    arguments: json!({"i": 3}),
                }),
            ],
            outcome: CompletionOutcome {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        error: None,
                        id: "c1".into(),
                        name: "a".into(),
                        arguments: json!({"i": 1}),
                    },
                    ToolCall {
                        error: None,
                        id: "c2".into(),
                        name: "b".into(),
                        arguments: json!({"i": 2}),
                    },
                    ToolCall {
                        error: None,
                        id: "c3".into(),
                        name: "c".into(),
                        arguments: json!({"i": 3}),
                    },
                ],
            },
        };
        stub.push_script(three_calls);

        let session = manager.create(ChatSessionBuilder::new().with_ephemeral(true));
        let outcome = session
            .run(
                Arc::new(std::collections::HashMap::new()),
                Vec::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                Some(2),
                Box::new(|_| Ok(())),
            )
            .await
            .expect("run should succeed");
        assert_eq!(outcome.tool_calls.len(), 2);
        assert_eq!(outcome.tool_calls[0].id, "c1");
        assert_eq!(outcome.tool_calls[1].id, "c2");
    }

    #[tokio::test]
    async fn no_cap_preserves_all_calls() {
        let (manager, _store, stub) = build_manager().await;
        let three_calls = StubScript {
            events: vec![
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c1".into(),
                    name: "a".into(),
                    arguments: json!({}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c2".into(),
                    name: "b".into(),
                    arguments: json!({}),
                }),
                StreamEvent::ToolCall(ToolCall {
                    error: None,
                    id: "c3".into(),
                    name: "c".into(),
                    arguments: json!({}),
                }),
            ],
            outcome: CompletionOutcome {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        error: None,
                        id: "c1".into(),
                        name: "a".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        error: None,
                        id: "c2".into(),
                        name: "b".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        error: None,
                        id: "c3".into(),
                        name: "c".into(),
                        arguments: json!({}),
                    },
                ],
            },
        };
        stub.push_script(three_calls);

        let session = manager.create(ChatSessionBuilder::new().with_ephemeral(true));
        let outcome = session
            .run(
                Arc::new(std::collections::HashMap::new()),
                Vec::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
                Box::new(|_| Ok(())),
            )
            .await
            .expect("run should succeed");
        assert_eq!(outcome.tool_calls.len(), 3);
    }

    #[tokio::test]
    async fn pre_cancelled_token_returns_chaterror_cancelled() {
        use frances_models_llm::chat::ChatError;
        use tokio_util::sync::CancellationToken;

        let (manager, _store, stub) = build_manager().await;
        // Script a successful turn that the provider should *never* see.
        stub.push_script(assistant_script("should not be reached"));

        let session = manager.create(ChatSessionBuilder::new().with_ephemeral(true));
        session.push(OwnedHistoryInput::User {
            text: "doomed".to_owned(),
        });

        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = session
            .run(
                Arc::new(std::collections::HashMap::new()),
                Vec::new(),
                None,
                cancel,
                None,
                Box::new(|_| Ok(())),
            )
            .await;

        assert!(
            matches!(result, Err(ChatError::Cancelled)),
            "expected Cancelled, got {result:?}",
        );
        // Provider never observed the request — the stub's bail check
        // fires before it pushes onto `requests`.
        assert!(
            stub.captured().is_empty(),
            "provider should not have been invoked once token was pre-cancelled",
        );
    }

    fn enforce_req<'a>(
        env: &'a std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
        inputs: &'a [HistoryInput<'a>],
    ) -> CompleteRequest<'a> {
        CompleteRequest {
            intents: &["default"],
            session_id: "enforce-test",
            env,
            history: &[],
            new_inputs: inputs,
            tools: &[],
            tool_choice: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            max_tool_calls: Some(1),
        }
    }

    #[tokio::test]
    async fn run_flags_schema_invalid_tool_call() {
        let (manager, _store, stub) = build_manager().await;
        // `decide` called with the required `verdict` missing.
        stub.push_script(tool_call_script("c1", "decide", json!({ "reason": "x" })));

        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "verdict": { "type": "string", "enum": ["approve", "decline"] },
                "reason": { "type": "string" },
            },
            "required": ["verdict", "reason"],
        });
        let decide = vec![frances_models_llm::ToolDef::Function(
            frances_models_llm::ToolFunction {
                name: "decide".into(),
                description: String::new(),
                parameters: schema.clone(),
            },
        )];

        let session = manager.create(ChatSessionBuilder::new().with_ephemeral(true));
        let outcome = session
            .run(
                Arc::new(std::collections::HashMap::new()),
                decide,
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
                Box::new(|_| Ok(())),
            )
            .await
            .expect("run succeeds even with a bad call");
        // The call stays in `tool_calls` (it was emitted), flagged with its error.
        assert_eq!(outcome.tool_calls.len(), 1);
        let err = outcome.tool_calls[0]
            .error
            .as_ref()
            .expect("invalid args flagged");
        assert!(!err.message.is_empty());
        assert_eq!(err.expected_schema, schema);
    }

    #[tokio::test]
    async fn complete_enforced_scolds_once_then_succeeds() {
        let (manager, _store, stub) = build_manager().await;
        // Round 1: a bare assistant turn (no tool call) → one scold.
        stub.push_script(assistant_script("thinking out loud"));
        // Round 2: the demanded `decide` call.
        stub.push_script(tool_call_script(
            "c1",
            "decide",
            json!({ "verdict": "approve" }),
        ));

        let env = std::collections::HashMap::new();
        let inputs = [HistoryInput::User { text: "may I?" }];
        let outcome = manager
            .complete_enforced(
                enforce_req(&env, &inputs),
                Demand::Function("decide".into()),
                1,
            )
            .await
            .expect("a decide call lands on the retry");
        assert!(outcome.tool_calls.iter().any(|c| c.name == "decide"));

        // Two rounds happened, and the second carried an extra (scold) input.
        let caps = stub.captured();
        assert_eq!(caps.len(), 2, "expected one scold + one success");
        assert!(
            caps[1].new_inputs.len() > caps[0].new_inputs.len(),
            "the scold should have been appended on the retry",
        );
    }

    #[tokio::test]
    async fn complete_enforced_retries_on_invalid_args_then_succeeds() {
        let (manager, _store, stub) = build_manager().await;
        // Round 1: `decide` is called, but the args are missing the required
        // `verdict` — the chat layer flags it, so the demand isn't satisfied.
        stub.push_script(tool_call_script("c1", "decide", json!({ "reason": "x" })));
        // Round 2: a well-formed `decide` call.
        stub.push_script(tool_call_script(
            "c2",
            "decide",
            json!({ "verdict": "approve", "reason": "ok" }),
        ));

        // Declared first so its borrow outlives the request built below.
        let decide = [frances_models_llm::ToolDef::Function(
            frances_models_llm::ToolFunction {
                name: "decide".into(),
                description: String::new(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "verdict": { "type": "string", "enum": ["approve", "decline"] },
                        "reason": { "type": "string" },
                    },
                    "required": ["verdict", "reason"],
                }),
            },
        )];
        let env = std::collections::HashMap::new();
        let inputs = [HistoryInput::User { text: "may I?" }];
        let mut req = enforce_req(&env, &inputs);
        req.tools = &decide;
        let outcome = manager
            .complete_enforced(req, Demand::Function("decide".into()), 1)
            .await
            .expect("the valid retry satisfies the demand");
        assert!(
            outcome
                .tool_calls
                .iter()
                .any(|c| c.name == "decide" && c.error.is_none()),
            "the satisfying call validates cleanly",
        );

        let caps = stub.captured();
        assert_eq!(caps.len(), 2, "one bad-args round + one success");
        assert!(
            caps[1].new_inputs.len() > caps[0].new_inputs.len(),
            "the scold (carrying the validation error) is appended on retry",
        );
    }

    #[tokio::test]
    async fn complete_enforced_unsatisfied_after_budget() {
        let (manager, _store, stub) = build_manager().await;
        // Never calls a tool; with retries = 1 that's two rounds total.
        stub.push_script(assistant_script("nope"));
        stub.push_script(assistant_script("still nope"));

        let env = std::collections::HashMap::new();
        let inputs = [HistoryInput::User { text: "may I?" }];
        let err = manager
            .complete_enforced(
                enforce_req(&env, &inputs),
                Demand::Function("decide".into()),
                1,
            )
            .await
            .expect_err("never satisfies the demand");
        assert!(
            matches!(err, EnforceError::Unsatisfied { .. }),
            "got {err:?}"
        );
        assert_eq!(stub.captured().len(), 2, "initial round + one retry");
    }
}
