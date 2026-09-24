use super::mcp::{McpError, McpTools};
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
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) enum Input {
    Prompt(String),
    Interrupt,
    SelectMcp {
        selection: frances_mcp::Selection,
        reply: oneshot::Sender<Result<()>>,
    },
    ListMcpPrompts {
        server: String,
        reply: oneshot::Sender<Result<serde_json::Value>>,
    },
    UseMcpPrompt {
        server: String,
        name: String,
        arguments: std::collections::BTreeMap<String, String>,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// A context owns the model conversation and its fixed tool inventory. Session
/// UI history and the shared anchor engine outlive this value. Future controller
/// context replacement belongs at the boundary between completed batches here.
struct Context<Io: HarnessIo> {
    chat: frances_llm::ChatSession<ChatDepsImpl>,
    tools: Tools<HarnessDepsImpl<Io>>,
    instructions: String,
    mcp: McpTools,
    outputs: Outputs,
}

pub(super) async fn run<Io: HarnessIo>(
    runtime: Arc<SessionRuntime<Io>>,
    mut input: mpsc::UnboundedReceiver<Input>,
    mcp: McpTools,
) {
    let (tx, mut output) = mpsc::unbounded_channel();
    let outputs = Outputs(tx);
    let mut context = Context {
        chat: runtime.chat.create(ChatSessionBuilder::new()),
        tools: Tools::new(runtime.deps.clone(), outputs.clone()),
        instructions: frances_harness::instructions::load(&runtime.deps).await,
        mcp,
        outputs: outputs.clone(),
    };
    let mut queued: VecDeque<Input> = VecDeque::new();
    let mut paused = false;
    loop {
        let next = queued
            .iter()
            .position(|item| !paused || !matches!(item, Input::Prompt(_)));
        let work = if let Some(index) = next {
            queued.remove(index).expect("queued input exists")
        } else {
            tokio::select! {
                biased;
                () = runtime.cancel.cancelled() => break,
                message = input.recv() => match message {
                    Some(Input::Interrupt) => { paused = true; continue; },
                    Some(message) => {
                        if matches!(message, Input::Prompt(_)) { paused = false; }
                        queued.push_back(message); continue;
                    },
                    None => break,
                }
            }
        };
        let cancel = runtime.cancel.child_token();
        runtime
            .entities
            .update_session(|s| {
                s.busy = Some(
                    match &work {
                        Input::SelectMcp { .. } => "Connecting MCP servers",
                        Input::ListMcpPrompts { .. } | Input::UseMcpPrompt { .. } => {
                            "Loading MCP prompt"
                        }
                        _ => "Working",
                    }
                    .into(),
                )
            })
            .await;
        let turn = handle_input(&runtime, &mut context, work, &cancel, &outputs);
        {
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    () = runtime.cancel.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
                    message = input.recv(), if !input.is_closed() => match message {
                        Some(Input::Prompt(text)) => { queued.push_back(Input::Prompt(text)); paused = false; },
                        Some(Input::Interrupt) => { cancel.cancel(); paused = true; },
                        Some(message) => { if matches!(message, Input::SelectMcp { .. }) { cancel.cancel(); } queued.push_back(message); },
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
    context.mcp.close().await;
    while let Ok(event) = output.try_recv() {
        publish(&runtime, event, &runtime.cancel).await;
    }
}

async fn handle_input<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    context: &mut Context<Io>,
    input: Input,
    cancel: &CancellationToken,
    outputs: &Outputs,
) -> Result<()> {
    match input {
        Input::Prompt(text) => turn(runtime, context, text, cancel, outputs).await,
        Input::Interrupt => Ok(()),
        Input::SelectMcp { selection, reply } => {
            let result = replace_mcp(runtime, context, selection, cancel, outputs).await;
            if let Err(error) = reply.send(result) {
                tracing::debug!(?error, "MCP selection caller closed");
            }
            Ok(())
        }
        Input::ListMcpPrompts { server, reply } => {
            let result = async {
                let connection = context.mcp.connections.get(&server).ok_or_else(|| {
                    McpError::Config(frances_mcp::Error::UnknownServer(server.clone()))
                })?;
                let prompts = if connection.has_prompts() {
                    connection.prompts(cancel).await.map_err(McpError::from)?
                } else {
                    vec![]
                };
                Ok(serde_json::to_value(prompts).map_err(McpError::from)?)
            }
            .await;
            if let Err(error) = reply.send(result) {
                tracing::debug!(?error, "MCP prompt caller closed");
            }
            Ok(())
        }
        Input::UseMcpPrompt {
            server,
            name,
            arguments,
            reply,
        } => {
            let result = async {
                let connection = context.mcp.connections.get(&server).ok_or_else(|| {
                    McpError::Config(frances_mcp::Error::UnknownServer(server.clone()))
                })?;
                let prompt = connection
                    .prompt(name.clone(), arguments, cancel)
                    .await
                    .map_err(McpError::from)?;
                let content = super::mcp_content::prompt(&prompt);
                turn(
                    runtime,
                    context,
                    format!("Use MCP prompt {server}/{name}:\n{content}"),
                    cancel,
                    outputs,
                )
                .await
            }
            .await;
            if let Err(error) = reply.send(result) {
                tracing::debug!(?error, "MCP prompt caller closed");
            }
            Ok(())
        }
    }
}

async fn replace_mcp<Io: HarnessIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    context: &mut Context<Io>,
    selection: frances_mcp::Selection,
    cancel: &CancellationToken,
    outputs: &Outputs,
) -> Result<()> {
    let env = {
        let invocation = runtime.invocation.lock();
        frances_mcp::Environment {
            worker: runtime.deps.io.worker().cloned(),
            cwd: invocation.workspace.primary_dir().to_path_buf(),
            env: invocation.process.env.clone(),
        }
    };
    // Prepare first. A failed connection leaves the current selection usable.
    let next = context
        .mcp
        .select(&runtime.mcp_config, &selection, &env, cancel)
        .await?;
    context.chat.persist_pending().await?;
    let history = match context.chat.id() {
        Some(id) => runtime.history.load_primitives(id).await?,
        None => vec![],
    };
    let chat = runtime.chat.create(ChatSessionBuilder::new());
    if !history.is_empty() {
        // Carry text history without nesting serialized transcripts on every switch.
        // Old tool exchanges are reference text, not calls under the new inventory.
        for input in history {
            let input = match input {
                OwnedHistoryInput::System { .. } => continue,
                OwnedHistoryInput::ToolCall {
                    name, arguments, ..
                } => OwnedHistoryInput::Assistant {
                    text: format!("Previous tool call: {name} {arguments}"),
                },
                OwnedHistoryInput::ToolResult {
                    content, is_error, ..
                } => OwnedHistoryInput::User {
                    text: format!("Previous tool result (error: {is_error}):\n{content}"),
                },
                input => input,
            };
            chat.push(input);
        }
        chat.push(OwnedHistoryInput::User { text: "The user changed the MCP selection. Continue using the current tool definitions. Historical tool calls are reference text; read files again before editing in this new context.".into() });
        chat.persist_pending().await?;
    }
    if cancel.is_cancelled() {
        return Err(McpError::Interrupted.into());
    }
    context.tools.stop_shell().await;
    context.tools.commit_edits().await?;
    let status = next.status(&runtime.mcp_config, selection);
    let previous = std::mem::replace(&mut context.mcp, next);
    context.chat = chat;
    context.tools = Tools::new(runtime.deps.clone(), outputs.clone());
    context.instructions = frances_harness::instructions::load(&runtime.deps).await;
    runtime
        .entities
        .update_session(|session| session.mcp = status)
        .await;
    previous.close().await;
    Ok(())
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
            text: format!("{}{}", context.instructions, context.mcp.instructions()),
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
            let (content, is_error) = if context.mcp.contains(&call.name) {
                match context.mcp.execute(&call, cancel, &context.outputs).await {
                    Ok(result) => result,
                    Err(error) => (error.to_string(), true),
                }
            } else {
                match context.tools.execute(&call, cancel).await {
                    Ok(content) => (content, false),
                    Err(error) => (error.to_string(), true),
                }
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
        context
            .tools
            .definitions()
            .iter()
            .cloned()
            .chain(context.mcp.definitions())
            .collect(),
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
