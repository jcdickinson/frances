use super::{ChatDepsImpl, HarnessDepsImpl, SessionRuntime};
use crate::{
    Result,
    events::{EntityEnvelope, Lifecycle, SectionKind, StreamFrame},
};
use frances_harness::{HarnessIo, Output, Outputs, Tools};
use frances_models_llm::chat::{
    ChatError, ChatSession as _, ChatSessionBuilder, ChatSessionManager as _,
};
use frances_models_llm::{CompletionOutcome, OwnedHistoryInput, StreamEvent};
use serde_json::json;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) enum Input {
    Prompt(String),
    Interrupt,
}

/// A context owns the model conversation and its fixed tool inventory. Session
/// UI history and the shared anchor engine outlive this value. Future controller
/// context replacement belongs at the boundary between completed batches here.
struct Context<Io: HarnessIo> {
    chat: frances_llm::ChatSession<ChatDepsImpl>,
    tools: Tools<HarnessDepsImpl<Io>>,
    instructions: String,
}

pub(super) async fn run<Io: HarnessIo>(
    runtime: Arc<SessionRuntime<Io>>,
    mut input: mpsc::UnboundedReceiver<Input>,
) {
    let (tx, mut output) = mpsc::unbounded_channel();
    let outputs = Outputs(tx);
    let mut context = Context {
        chat: runtime.chat.create(ChatSessionBuilder::new()),
        tools: Tools::new(runtime.deps.clone(), outputs.clone()),
        instructions: frances_harness::instructions::load(&runtime.deps).await,
    };
    let mut queued = VecDeque::new();
    let mut paused = false;
    loop {
        let text = if !paused && !queued.is_empty() {
            queued.pop_front().expect("queue is nonempty")
        } else {
            tokio::select! {
                biased;
                () = runtime.cancel.cancelled() => break,
                message = input.recv() => match message {
                    Some(Input::Prompt(text)) => { paused = false; queued.push_back(text); continue; },
                    Some(Input::Interrupt) => { paused = true; continue; },
                    None => break,
                }
            }
        };
        let cancel = runtime.cancel.child_token();
        runtime
            .entities
            .update_session(|s| s.busy = Some("Working".into()))
            .await;
        let turn = turn(&runtime, &mut context, text, &cancel, &outputs);
        {
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    () = runtime.cancel.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
                    message = input.recv(), if !input.is_closed() => match message {
                        Some(Input::Prompt(text)) => { queued.push_back(text); paused = false; },
                        Some(Input::Interrupt) => { cancel.cancel(); paused = true; },
                        None => cancel.cancel(),
                    },
                    Some(event) = output.recv() => publish(&runtime, event, &cancel).await,
                    result = &mut turn => {
                        if let Err(error) = result {
                            if cancel.is_cancelled() {
                                tracing::debug!(%error, "agent turn interrupted");
                            } else {
                                tracing::warn!(%error, "agent turn failed");
                                runtime.events.send(StreamFrame::Error(error.to_string()));
                            }
                        }
                        break;
                    }
                }
            }
        }
        while let Ok(event) = output.try_recv() {
            publish(&runtime, event, &cancel).await;
        }
        runtime.entities.update_session(|s| s.busy = None).await;
        if runtime.cancel.is_cancelled() {
            break;
        }
    }
    context.tools.stop_shell().await;
    while let Ok(event) = output.try_recv() {
        publish(&runtime, event, &runtime.cancel).await;
    }
}

async fn turn<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    context: &mut Context<Io>,
    text: String,
    cancel: &CancellationToken,
    output: &Outputs,
) -> Result<()> {
    if runtime.entities.session_title().is_none() {
        let title: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();
        if !title.is_empty() {
            if let Err(error) = runtime.session.write_title(Some(title.clone())) {
                tracing::warn!(%error, "persist session title failed");
            }
            runtime
                .entities
                .update_session(|session| session.title = Some(title))
                .await;
        }
    }
    let snapshot = json!({"source":"user", "text":text});
    let id = output.open("chat", snapshot.clone());
    output.settle(id, snapshot);
    context.chat.push(OwnedHistoryInput::User { text });
    context.chat.persist_pending().await?;
    let result = drive(runtime, context, cancel).await;
    context.tools.stop_shell().await;
    context.chat.persist_pending().await?;
    context.tools.commit_edits().await?;
    result
}

async fn drive<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    context: &mut Context<Io>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut shell_reminders = 0;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        context.chat.push_system(OwnedHistoryInput::System {
            text: context.instructions.clone(),
        });
        let outcome = model_call(runtime, context, cancel).await?;
        if outcome.tool_calls.is_empty() {
            if !context.tools.shell_running() {
                return Ok(());
            }
            if shell_reminders >= 2 {
                context.tools.stop_shell().await;
                context.chat.push(OwnedHistoryInput::User {
                    text: "The unattended shell command was killed. Report the result to the user."
                        .into(),
                });
            } else {
                context.chat.push(OwnedHistoryInput::User {
                    text: "A shell command is still running. Call shell_wait or shell_kill.".into(),
                });
                shell_reminders += 1;
            }
            continue;
        }
        shell_reminders = 0;
        // Every call receives a result, even if a prior call was interrupted.
        // No new model request is issued while a batch is unsettled.
        for call in outcome.tool_calls {
            let result = context.tools.execute(&call, cancel).await;
            let (content, is_error) = match result {
                Ok(content) => (content, false),
                Err(error) => (error.to_string(), true),
            };
            context.chat.push(OwnedHistoryInput::ToolResult {
                call_id: call.id,
                content,
                is_error,
            });
        }
        context.chat.persist_pending().await?;
        context.tools.commit_edits().await?;
    }
}

async fn model_call<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    context: &mut Context<Io>,
    cancel: &CancellationToken,
) -> Result<CompletionOutcome> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let env = runtime.invocation.lock().process.env.clone();
    let request = context.chat.run(
        env,
        context.tools.definitions().to_vec(),
        None,
        cancel.clone(),
        None,
        Box::new(move |event| tx.send(event).map_err(|_| ChatError::Cancelled)),
    );
    let mut assistant = Message::new("assistant");
    let mut reasoning = Message::new("reasoning");
    let result = {
        tokio::pin!(request);
        loop {
            tokio::select! {
                biased;
                Some(event) = rx.recv() => model_event(runtime, event, &mut assistant, &mut reasoning).await?,
                result = &mut request => break result,
            }
        }
    };
    while let Ok(event) = rx.try_recv() {
        model_event(runtime, event, &mut assistant, &mut reasoning).await?;
    }
    assistant.settle(runtime).await?;
    reasoning.settle(runtime).await?;
    Ok(result?)
}

struct Message {
    id: Option<Uuid>,
    source: &'static str,
    text: String,
}
impl Message {
    fn new(source: &'static str) -> Self {
        Self {
            id: None,
            source,
            text: String::new(),
        }
    }
    async fn append<Io: HarnessIo>(
        &mut self,
        runtime: &SessionRuntime<Io>,
        delta: &str,
    ) -> Result<()> {
        self.text.push_str(delta);
        let fresh = self.id.is_none();
        let id = *self.id.get_or_insert_with(Uuid::new_v4);
        runtime
            .entities
            .upsert_snapshot(
                EntityEnvelope {
                    entity_id: id,
                    kind: "chat".into(),
                    lifecycle: Lifecycle::Live,
                },
                json!({"source":self.source, "text":self.text}),
            )
            .await?;
        if fresh {
            section(runtime, SectionKind::EntityRef { entity_id: id }).await?;
        }
        Ok(())
    }
    async fn settle<Io: HarnessIo>(&self, runtime: &SessionRuntime<Io>) -> Result<()> {
        if let Some(id) = self.id {
            runtime
                .entities
                .settle(
                    id,
                    json!({"source":self.source,"text":self.text}),
                    vec![],
                    None,
                )
                .await?;
        }
        Ok(())
    }
}
async fn model_event<Io: HarnessIo>(
    runtime: &SessionRuntime<Io>,
    event: StreamEvent,
    assistant: &mut Message,
    reasoning: &mut Message,
) -> Result<()> {
    match event {
        StreamEvent::TextDelta(text) => assistant.append(runtime, &text).await?,
        StreamEvent::ReasoningDelta(text) => reasoning.append(runtime, &text).await?,
        StreamEvent::Usage(usage) => {
            runtime
                .entities
                .update_session(|s| s.usage = Some(usage))
                .await
        }
        StreamEvent::ToolCall(_) | StreamEvent::History(_) => {}
    }
    Ok(())
}
async fn section<Io: HarnessIo>(runtime: &SessionRuntime<Io>, section: SectionKind) -> Result<()> {
    crate::scrollback::persist_section(&runtime.db, &section).await?;
    runtime.events.send(StreamFrame::Section(section));
    Ok(())
}
async fn publish<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    output: Output,
    cancel: &CancellationToken,
) {
    let result: Result<()> = async {
        match output {
            Output::Section(value) => section(runtime, value).await?,
            Output::Snapshot { id, kind, snapshot } => {
                runtime
                    .entities
                    .upsert_snapshot(
                        EntityEnvelope {
                            entity_id: id,
                            kind,
                            lifecycle: Lifecycle::Live,
                        },
                        snapshot,
                    )
                    .await?
            }
            Output::Append { id, payload } => runtime.entities.append(id, payload).await?,
            Output::Settle {
                id,
                snapshot,
                artifacts,
            } => {
                runtime
                    .entities
                    .settle(id, snapshot, artifacts, None)
                    .await?
            }
            Output::Permission(request) => {
                let runtime = runtime.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => {},
                        () = permission(&runtime, request, &cancel) => {},
                    }
                });
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "publish agent output failed");
        runtime.events.send(StreamFrame::Error(error.to_string()));
    }
}
async fn permission<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    request: frances_harness::PermissionRequest,
    cancel: &CancellationToken,
) {
    if request.allow_auto {
        match super::auto_judge::judge(runtime, &request, cancel.clone()).await {
            super::auto_judge::JudgeOutcome::Approve { reason } => {
                if let Err(error) = request
                    .reply
                    .send(frances_harness::PermissionResponse::Yes {
                        details: Some(reason),
                    })
                {
                    tracing::debug!(?error, "permission receiver closed");
                }
                return;
            }
            super::auto_judge::JudgeOutcome::Reject { reason }
            | super::auto_judge::JudgeOutcome::Indeterminate { reason } => {
                tracing::debug!(%reason, "permission requires user response")
            }
        }
    }
    if !cancel.is_cancelled() {
        runtime.events.send(StreamFrame::Permission(request));
    }
}
