//! Test fixtures for downstream crates (and this crate's own unit tests).
//!
//! `StubProvider` lets a test script the provider's wire-level responses
//! and inspect each `ProviderRequest` after the fact. `ProviderCache`
//! gains an [`insert_stub`](crate::ProviderCache::insert_stub) method
//! that drops a `StubProvider` straight into the cache so callers never
//! exercise the hard-coded OpenAI build path.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use frances_config::ConfigHandle;
use parking_lot::Mutex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use frances_models_llm::chat::OwnedHistoryInput;
use frances_models_llm::config::ProviderConfig;
use frances_models_llm::{CompletionOutcome, ErasedError, HistoryInput, StreamEvent};

use crate::provider::{Provider, ProviderRequest};

/// A single scripted provider response.
#[derive(Clone)]
pub struct StubScript {
    pub events: Vec<StreamEvent>,
    pub outcome: CompletionOutcome,
}

impl StubScript {
    /// An empty assistant turn — no events, no text, no tool calls.
    pub fn empty() -> Self {
        Self {
            events: Vec::new(),
            outcome: CompletionOutcome {
                text: String::new(),
                tool_calls: Vec::new(),
            },
        }
    }
}

/// What was passed to one `stream()` call. Owned so the test can keep
/// the value past the original borrow's lifetime.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub session_id: String,
    pub history: Vec<Value>,
    pub new_inputs: Vec<OwnedHistoryInput>,
}

/// A `Provider` impl driven by a script queue. Each `stream()` call
/// pops the next script; if the queue is empty, the call returns an
/// error so test mis-wirings fail loudly.
pub struct StubProvider {
    scripts: Mutex<VecDeque<StubScript>>,
    requests: Mutex<Vec<CapturedRequest>>,
}

impl Default for StubProvider {
    fn default() -> Self {
        Self {
            scripts: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl StubProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_script(&self, script: StubScript) {
        self.scripts.lock().push_back(script);
    }

    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.requests.lock().clone()
    }
}

fn history_input_into_owned(input: &HistoryInput<'_>) -> OwnedHistoryInput {
    match *input {
        HistoryInput::System { text } => OwnedHistoryInput::System {
            text: text.to_owned(),
        },
        HistoryInput::User { text } => OwnedHistoryInput::User {
            text: text.to_owned(),
        },
        HistoryInput::Assistant { text } => OwnedHistoryInput::Assistant {
            text: text.to_owned(),
        },
        HistoryInput::ToolCall {
            id,
            name,
            arguments,
        } => OwnedHistoryInput::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.clone(),
        },
        HistoryInput::ToolResult {
            call_id,
            content,
            is_error,
        } => OwnedHistoryInput::ToolResult {
            call_id: call_id.to_owned(),
            content: content.to_owned(),
            is_error,
        },
    }
}

#[async_trait]
impl Provider for StubProvider {
    type BuildError = ErasedError;
    type Error = ErasedError;

    fn kind(&self) -> &'static str {
        "stub"
    }

    fn new(_: ProviderConfig, _: ConfigHandle) -> Result<Arc<Self>, Self::BuildError> {
        Ok(Arc::new(Self::new()))
    }

    fn forge_history(&self, _: &[HistoryInput<'_>]) -> Vec<Value> {
        Vec::new()
    }

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        cancel: CancellationToken,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), Self::Error> + Send),
    ) -> Result<CompletionOutcome, Self::Error> {
        if cancel.is_cancelled() {
            return Err(Box::<dyn std::error::Error + Send + Sync>::from(
                "stub provider: cancelled before stream",
            ));
        }
        self.requests.lock().push(CapturedRequest {
            session_id: req.session_id.to_owned(),
            history: req.history.to_vec(),
            new_inputs: req
                .new_inputs
                .iter()
                .map(history_input_into_owned)
                .collect(),
        });
        let script = self
            .scripts
            .lock()
            .pop_front()
            .ok_or_else(|| -> ErasedError {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "StubProvider::stream called with no script queued",
                )
            })?;
        let cap = req.max_tool_calls;
        let mut emitted_calls: Vec<frances_models_llm::ToolCall> = Vec::new();
        for ev in script.events {
            // Mirror the OpenAI provider's truncation: once we've
            // emitted `cap` tool calls, drop everything further on the
            // floor and return Ok with what we have. Lets tests
            // exercise the cap path without standing up a real wire.
            if let Some(cap) = cap
                && emitted_calls.len() >= cap
            {
                break;
            }
            if let StreamEvent::ToolCall(call) = &ev {
                emitted_calls.push(call.clone());
            }
            on_event(ev)?;
        }
        if let Some(cap) = cap
            && script.outcome.tool_calls.len() > cap
        {
            // Match the SSE-loop's contract: outcome.tool_calls
            // reflects only what we actually emitted.
            let mut truncated = script.outcome;
            truncated.tool_calls.truncate(cap);
            return Ok(truncated);
        }
        Ok(script.outcome)
    }
}
