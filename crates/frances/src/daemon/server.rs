use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tarpc::context;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, oneshot};
use tracing::{debug, info, trace, warn};

use crate::anchor_store::AnchorStoreImpl;
use crate::context::InvocationContext;
use crate::daemon::client::{read_message, remove_socket_if_present, write_message};
use crate::daemon::protocol::{
    AttachResponse, BlockId, BlockKind, Client, DaemonPid, DaemonStatus, PROTOCOL_VERSION,
    PromptId, SessionId, StreamFrame,
};
use crate::edit_session::EditSession;
use crate::history::{Block, HistoryStore, Role};
use crate::llm::{
    self, ChatClient, ModelConfig, ProviderConfig, ResponsesModelExtras, SessionConfigProvider,
    SessionConfigWriter, ToolCallAccumulator,
};
use crate::session::Session;
use crate::shell_classifier::{self, ShellClassification};
use crate::store::Database;
use crate::tools::{self, ToolRegistry};
use crate::workflows::{self, WorkflowConfig};
use frances_config::{
    ConfigBinding, ConfigHandle, ConfigProvider, EnvProvider, RequiredConfigBinding, TomlProvider,
};
use frances_edit::EditEngine;
use frances_shell::Shell;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

pub(crate) struct ServerState {
    session: Session,
    client_attached: StdMutex<bool>,
    last_context: StdMutex<Option<InvocationContext>>,
    daemon_pid: u32,
    history: HistoryStore,
    edit_session: tokio::sync::Mutex<EditSession<AnchorStoreImpl>>,
    shell: tokio::sync::Mutex<Option<Shell>>,
    tool_registry: ToolRegistry,
    events: EventsRouter,
    shutdown: Notify,
    /// Kept alive so the config-event-processor task stays running for the
    /// daemon's lifetime. Also handed to `ChatClient` so it can lazily bind
    /// `models::<name>` on demand when callers request a named model.
    config: ConfigHandle,
    providers: ConfigBinding<HashMap<String, ProviderConfig>>,
    default_model: RequiredConfigBinding<ModelConfig>,
    responses_extras: ConfigBinding<HashMap<String, ResponsesModelExtras>>,
    workflows: ConfigBinding<HashMap<String, WorkflowConfig>>,
    /// Writes session-config rows and emits the matching events on the
    /// DB layer in one call. Held for future RPC handlers that mutate
    /// session config.
    #[expect(dead_code, reason = "wired for future session-config writers")]
    session_config_writer: SessionConfigWriter,
}

#[derive(Default)]
struct EventsRouter {
    inner: StdMutex<HashMap<PromptId, EventsSlot>>,
}

enum EventsSlot {
    HasStream(UnixStream),
    Waiting(oneshot::Sender<UnixStream>),
}

impl EventsRouter {
    fn register(&self, id: PromptId, stream: UnixStream) {
        let mut inner = self.inner.lock().expect("events router poisoned");
        match inner.remove(&id) {
            Some(EventsSlot::Waiting(tx)) => {
                let _ = tx.send(stream);
            }
            Some(EventsSlot::HasStream(_)) | None => {
                inner.insert(id, EventsSlot::HasStream(stream));
            }
        }
    }

    async fn take(&self, id: PromptId) -> Option<UnixStream> {
        let rx = {
            let mut inner = self.inner.lock().expect("events router poisoned");
            match inner.remove(&id) {
                Some(EventsSlot::HasStream(s)) => return Some(s),
                Some(EventsSlot::Waiting(_)) => return None,
                None => {
                    let (tx, rx) = oneshot::channel();
                    inner.insert(id, EventsSlot::Waiting(tx));
                    rx
                }
            }
        };
        match tokio::time::timeout(EVENTS_PAIRING_TIMEOUT, rx).await {
            Ok(Ok(stream)) => Some(stream),
            _ => {
                self.inner
                    .lock()
                    .expect("events router poisoned")
                    .remove(&id);
                None
            }
        }
    }
}

#[derive(Clone)]
struct ClientServer {
    state: Arc<ServerState>,
}

impl Client for ClientServer {
    async fn attach(self, _: context::Context, ctx: InvocationContext) -> AttachResponse {
        trace!(
            session_id = %self.state.session.id,
            env_vars = ctx.process.env.len(),
            has_cwd = ctx.process.cwd.is_some(),
            "received attach context"
        );
        let mut attached = self
            .state
            .client_attached
            .lock()
            .expect("client_attached poisoned");
        if *attached {
            AttachResponse::Busy
        } else {
            *self
                .state
                .last_context
                .lock()
                .expect("last_context poisoned") = Some(ctx);
            *attached = true;
            AttachResponse::Attached {
                session_id: SessionId(self.state.session.id.clone()),
            }
        }
    }

    async fn detach(self, _: context::Context) {
        let mut attached = self
            .state
            .client_attached
            .lock()
            .expect("client_attached poisoned");
        *attached = false;
    }

    async fn prompt(
        self,
        _: context::Context,
        prompt_id: PromptId,
        text: String,
    ) -> Result<(), String> {
        let stream = self
            .state
            .events
            .take(prompt_id)
            .await
            .ok_or_else(|| format!("no events socket registered for prompt {prompt_id}"))?;

        let state = self.state.clone();
        tokio::spawn(async move {
            run_prompt(state, stream, text).await;
        });
        Ok(())
    }
}

pub fn install_logging(session: &Session) -> Result<()> {
    let log_path = session.dir.join("daemon.log");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;

    let fd = file.as_raw_fd();
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) < 0 {
            return Err(io::Error::last_os_error()).context("dup2 stdout to daemon log");
        }
        if libc::dup2(fd, libc::STDERR_FILENO) < 0 {
            return Err(io::Error::last_os_error()).context("dup2 stderr to daemon log");
        }
    }
    drop(file);

    // Default to warn for the world; raise frances/frances-edit/frances-anchors
    // /frances-config to trace so we can see our own logs without drowning in
    // turso/hyper/reqwest internals. Overridable via RUST_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "warn,frances=trace,frances_edit=trace,frances_anchors=trace,frances_config=trace",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))?;

    info!(session_id = %session.id, log = %log_path.display(), "daemon logging installed");
    Ok(())
}

pub async fn run(session: Session, db: Database) -> Result<()> {
    debug!(session_id = %session.id, "starting daemon server");

    fs::create_dir_all(&session.runtime_dir).with_context(|| {
        format!(
            "failed to create runtime dir {}",
            session.runtime_dir.display()
        )
    })?;

    remove_socket_if_present(&session.control_socket_path())?;
    remove_socket_if_present(&session.client_socket_path())?;
    remove_socket_if_present(&session.events_socket_path())?;

    fs::write(session.pid_path(), std::process::id().to_string())
        .with_context(|| format!("failed writing pid file for {}", session.id))?;

    let edit_engine = EditEngine::new(AnchorStoreImpl::new(db.clone()));

    let session_provider = Arc::new(SessionConfigProvider::new(db.clone()));
    let config_providers = build_config_providers(session_provider.clone());
    let config = ConfigHandle::build(config_providers)
        .await
        .context("build config handle")?;
    let session_config_writer = session_provider
        .writer()
        .expect("SessionConfigProvider::load ran during ConfigHandle::build");
    let providers = config
        .bind::<HashMap<String, ProviderConfig>>("model_providers")
        .context("bind model_providers")?;
    let default_model = config
        .bind::<ModelConfig>(["models", "default"])
        .context("bind models::default")?
        .required()
        .context("models::default is required")?;
    let responses_extras = config
        .bind::<HashMap<String, ResponsesModelExtras>>("responses_models")
        .context("bind responses_models")?;
    let workflows = config
        .bind::<HashMap<String, WorkflowConfig>>("workflows")
        .context("bind workflows")?;

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: StdMutex::new(false),
        last_context: StdMutex::new(None),
        daemon_pid: std::process::id(),
        history: HistoryStore::new(db),
        edit_session: tokio::sync::Mutex::new(EditSession::new(edit_engine)),
        shell: tokio::sync::Mutex::new(None),
        tool_registry: ToolRegistry::builtin(),
        events: EventsRouter::default(),
        shutdown: Notify::new(),
        config,
        providers,
        default_model,
        responses_extras,
        workflows,
        session_config_writer,
    });

    let events_listener = UnixListener::bind(session.events_socket_path()).with_context(|| {
        format!(
            "failed to bind events socket {}",
            session.events_socket_path().display()
        )
    })?;
    let events_state = state.clone();
    tokio::spawn(async move {
        accept_events(events_listener, events_state).await;
    });

    let control_state = state.clone();
    let control_path = session.control_socket_path();
    tokio::spawn(async move {
        if let Err(error) = serve_control(control_path, control_state).await {
            warn!(%error, "control listener exited");
        }
    });

    let client_state = state.clone();
    let client_path = session.client_socket_path();
    tokio::spawn(async move {
        if let Err(error) = serve_client(client_path, client_state).await {
            warn!(%error, "client listener exited");
        }
    });

    info!(
        session_id = %session.id,
        control = %session.control_socket_path().display(),
        client = %session.client_socket_path().display(),
        events = %session.events_socket_path().display(),
        "daemon listening"
    );

    state.shutdown.notified().await;
    info!("shutdown signaled");

    let _ = fs::remove_file(session.pid_path());
    let _ = fs::remove_file(session.control_socket_path());
    let _ = fs::remove_file(session.client_socket_path());
    let _ = fs::remove_file(session.events_socket_path());

    Ok(())
}

// The control socket speaks a deliberately tiny newline-delimited TEXT protocol,
// not tarpc/bincode. Rationale: control is for management — "what version are
// you?" and "please shut down" — and it has to keep working *across binary
// versions* so a client built against a new schema can still ask an old daemon
// to step aside. Any binary serialization format (bincode, protobuf, etc.)
// breaks the moment a single field shape changes, which is exactly the
// situation the version-mismatch flow exists to handle. Plain text with
// `key=value` lines and an explicit terminator is forward-compatible by
// convention: unknown commands → `err`; unknown keys → ignored.
//
// On every accepted connection the server's first action is to write the
// current build's PROTOCOL_VERSION as a hex banner line. The client reads
// that line first and can decide to bail without sending any command.
//
// Wire shape:
//   server → client: "<protocol_version_hex>\n"
//   client → server: "<command>[ <args>]\n"
//   server → client: "ok\n" or "err <msg>\n"
//                    optional "key=value\n" lines
//                    "\n"  (blank line ends response)
async fn serve_control(path: std::path::PathBuf, state: Arc<ServerState>) -> Result<()> {
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind control socket {}", path.display()))?;
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                warn!(%error, "control accept error");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_control_conn(&mut stream, state).await {
                trace!(%error, "control handler exited");
            }
        });
    }
}

async fn handle_control_conn(stream: &mut UnixStream, state: Arc<ServerState>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.split();

    // Greet with our build's protocol id so the client can decide compatibility
    // before issuing any command.
    write_half
        .write_all(format!("{PROTOCOL_VERSION:016x}\n").as_bytes())
        .await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let request = line.trim();
    let mut parts = request.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    let response = match cmd {
        "ping" => "ok\n\n".to_string(),
        "status" => {
            let s = daemon_status(&state);
            let mut out = String::from("ok\n");
            out.push_str(&format!("session_id={}\n", s.session_id));
            out.push_str(&format!("client_attached={}\n", s.client_attached));
            out.push_str(&format!("daemon_pid={}\n", s.daemon_pid));
            out.push_str(&format!("protocol_version={:016x}\n", s.protocol_version));
            out.push('\n');
            out
        }
        "stop" => {
            let _delete_state = args.split_whitespace().any(|tok| tok == "delete=1");
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SHUTDOWN_GRACE).await;
                state.shutdown.notify_waiters();
            });
            "ok\n\n".to_string()
        }
        other => format!("err unknown command: {other}\n\n"),
    };

    write_half.write_all(response.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

async fn serve_client(path: std::path::PathBuf, state: Arc<ServerState>) -> Result<()> {
    let mut listener = tarpc::serde_transport::unix::listen(&path, Bincode::default).await?;
    listener.config_mut().max_frame_length(usize::MAX);
    while let Some(transport) = listener.next().await {
        let transport = match transport {
            Ok(t) => t,
            Err(error) => {
                warn!(%error, "client accept error");
                continue;
            }
        };
        let server = ClientServer {
            state: state.clone(),
        };
        let channel = tarpc::server::BaseChannel::with_defaults(transport);
        tokio::spawn(
            channel
                .execute(server.serve())
                .for_each(|response| async move {
                    tokio::spawn(response);
                }),
        );
    }
    Ok(())
}

async fn accept_events(listener: UnixListener, state: Arc<ServerState>) {
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let id: PromptId = match read_message(&mut stream).await {
                        Ok(id) => id,
                        Err(error) => {
                            warn!(%error, "events handshake failed");
                            return;
                        }
                    };
                    trace!(prompt_id = %id, "events socket registered");
                    state.events.register(id, stream);
                });
            }
            Err(error) => {
                warn!(%error, "events accept error");
                return;
            }
        }
    }
}

async fn run_prompt(state: Arc<ServerState>, mut stream: UnixStream, text: String) {
    if let Err(error) = stream_prompt(&state, &mut stream, text).await {
        warn!(%error, "prompt handler failed");
        match write_message(&mut stream, &StreamFrame::Error(format!("{error:#}"))).await {
            Ok(()) => trace!("wrote error frame"),
            Err(e) => warn!(error = %e, "failed to write error frame"),
        }
    }
    match write_message(&mut stream, &StreamFrame::Done).await {
        Ok(()) => trace!("wrote done frame"),
        Err(e) => warn!(error = %e, "failed to write done frame"),
    }
}

async fn stream_prompt(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: String,
) -> Result<()> {
    let result = run_handler(state, stream, text).await;
    if let Err(error) = state.edit_session.lock().await.end_turn().await {
        warn!(%error, "edit_session::end_turn failed");
    }
    result
}

async fn run_handler(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: String,
) -> Result<()> {
    let workflows = state.workflows.get_or_default();
    if workflows::dispatch_slash_command(&workflows, stream, &text).await? {
        return Ok(());
    }
    run_turn(state, stream, text).await
}

async fn run_turn(state: &Arc<ServerState>, stream: &mut UnixStream, text: String) -> Result<()> {
    let (env, cwd) = {
        let guard = state.last_context.lock().expect("last_context poisoned");
        let ctx = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no client context — attach first"))?;
        (ctx.process.env.clone(), ctx.process.cwd.clone())
    };

    let llm = ChatClient::new(
        env,
        state.providers.clone(),
        state.config.clone(),
        state.default_model.clone(),
        state.responses_extras.clone(),
    )?;

    let mut next_block: u64 = 1;
    let mut alloc_block = || {
        let id = BlockId(next_block);
        next_block += 1;
        id
    };

    let mut send_error: Option<anyhow::Error> = None;

    let user_block = Block::Text { text: text.clone() };
    let user_payload = serde_json::json!({ "role": "user", "content": text });
    state
        .history
        .append(Role::User, vec![user_block], user_payload)
        .await?;

    let user_id = alloc_block();
    for frame in [
        StreamFrame::BlockStart {
            id: user_id,
            kind: BlockKind::UserText,
        },
        StreamFrame::BlockDelta {
            id: user_id,
            text: text.clone(),
        },
        StreamFrame::BlockStop { id: user_id },
    ] {
        try_write(stream, &frame, &mut send_error).await;
    }

    let mut iterations: u32 = 0;
    loop {
        iterations += 1;
        if iterations.is_multiple_of(25) {
            warn!(iterations, "agent loop running long");
        }

        let made_tool_calls = run_llm_step(
            state,
            stream,
            &llm,
            &mut alloc_block,
            &mut send_error,
            cwd.as_deref(),
        )
        .await?;

        if !made_tool_calls {
            break;
        }
    }

    if let Some(error) = send_error {
        return Err(error);
    }
    Ok(())
}

/// Runs one LLM call, streams the result, persists the assistant message,
/// and dispatches any tool calls (also persisting their results). Returns
/// `true` if the model emitted tool calls (caller should loop), `false` if
/// the model's response was terminal.
async fn run_llm_step(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    llm: &ChatClient,
    alloc_block: &mut impl FnMut() -> BlockId,
    send_error: &mut Option<anyhow::Error>,
    cwd: Option<&std::path::Path>,
) -> Result<bool> {
    let assistant_message_id = state.history.start_assistant().await?;
    let payloads = state.history.openai_payloads().await?;

    let assistant_id = alloc_block();
    let mut wire_active: Option<BlockId> = Some(assistant_id);
    try_write(
        stream,
        &StreamFrame::BlockStart {
            id: assistant_id,
            kind: BlockKind::AssistantText,
        },
        send_error,
    )
    .await;

    let tool_defs = state.tool_registry.definitions().await?;

    let llm_owned: ChatClient = (*llm).clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let llm_task = tokio::spawn(async move {
        llm_owned
            .stream(
                &["chat"],
                &payloads,
                &tool_defs,
                None,
                move |chunk: &serde_json::Value| {
                    let _ = tx.send(chunk.clone());
                    Ok(())
                },
            )
            .await
    });

    let mut accumulated_text = String::new();
    let mut chunks: Vec<serde_json::Value> = Vec::new();
    let mut accumulator = ToolCallAccumulator::new();
    // streaming-index → wire BlockId for the most recently started tool block.
    // The TUI is single-active-block: a fresh BlockStart auto-closes the
    // previous block on the wire, so we never explicitly Stop them — the only
    // explicit Stop is for whichever block is `wire_active` when the stream ends.
    let mut tool_block_for_index: HashMap<u32, BlockId> = HashMap::new();

    while let Some(chunk) = rx.recv().await {
        for delta in llm::chunk_text_deltas(&chunk) {
            accumulated_text.push_str(delta);
            // Text deltas only make sense if the assistant block is still the
            // active one. Once a tool call has started, we drop further text
            // deltas on the wire (they'd address a closed block) — the model
            // rarely emits text after tool_calls in OpenAI's stream anyway.
            if wire_active == Some(assistant_id) {
                try_write(
                    stream,
                    &StreamFrame::BlockDelta {
                        id: assistant_id,
                        text: delta.to_string(),
                    },
                    send_error,
                )
                .await;
            }
        }
        for tool_delta in llm::chunk_tool_call_deltas(&chunk) {
            match &tool_delta.event {
                llm::ToolCallEvent::Start { name, .. } => {
                    let block_id = alloc_block();
                    tool_block_for_index.insert(tool_delta.index, block_id);
                    wire_active = Some(block_id);
                    try_write(
                        stream,
                        &StreamFrame::BlockStart {
                            id: block_id,
                            kind: BlockKind::ToolUse {
                                name: (*name).to_string(),
                            },
                        },
                        send_error,
                    )
                    .await;
                }
                llm::ToolCallEvent::Append(fragment) => {
                    if let Some(&block_id) = tool_block_for_index.get(&tool_delta.index)
                        && wire_active == Some(block_id)
                    {
                        try_write(
                            stream,
                            &StreamFrame::BlockDelta {
                                id: block_id,
                                text: (*fragment).to_string(),
                            },
                            send_error,
                        )
                        .await;
                    }
                }
            }
            accumulator.push(tool_delta)?;
        }
        if let Some(usage) = llm::chunk_usage(&chunk) {
            try_write(stream, &StreamFrame::Usage(usage), send_error).await;
        }
        chunks.push(chunk);
    }

    if let Some(id) = wire_active.take() {
        try_write(stream, &StreamFrame::BlockStop { id }, send_error).await;
    }

    let stream_result = llm_task.await.context("llm task panicked")?;
    stream_result?;

    let tool_calls = accumulator.finalize()?;

    let mut assistant_blocks: Vec<Block> = Vec::new();
    if !accumulated_text.is_empty() {
        assistant_blocks.push(Block::Text {
            text: accumulated_text.clone(),
        });
    }
    for call in &tool_calls {
        assistant_blocks.push(Block::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.arguments.clone(),
        });
    }
    let assistant_payload = tools::assistant_payload(&accumulated_text, &tool_calls);
    state
        .history
        .finish_assistant(assistant_message_id, assistant_blocks, assistant_payload)
        .await?;
    state
        .history
        .append_response_chunks(assistant_message_id, &chunks)
        .await?;

    if tool_calls.is_empty() {
        return Ok(false);
    }

    for call in &tool_calls {
        if call.name == "shell_run"
            && let Some(cmd) = call
                .arguments
                .get("cmd")
                .and_then(serde_json::Value::as_str)
        {
            let classification = shell_classifier::classify_shell(llm, cmd).await;
            let cls_id = alloc_block();
            emit_classification_block(stream, send_error, cls_id, &classification).await;
        }

        let outcome = state
            .tool_registry
            .dispatch(
                call,
                &tools::ToolContext {
                    edit_session: &state.edit_session,
                    shell: &state.shell,
                    cwd,
                },
            )
            .await;

        let result_id = alloc_block();
        try_write(
            stream,
            &StreamFrame::BlockStart {
                id: result_id,
                kind: BlockKind::ToolResult {
                    tool_use_id: call.id.clone(),
                    is_error: outcome.is_error,
                },
            },
            send_error,
        )
        .await;
        try_write(
            stream,
            &StreamFrame::BlockDelta {
                id: result_id,
                text: outcome.content.clone(),
            },
            send_error,
        )
        .await;
        try_write(
            stream,
            &StreamFrame::BlockStop { id: result_id },
            send_error,
        )
        .await;

        let result_block = Block::ToolResult {
            tool_use_id: call.id.clone(),
            content: outcome.content.clone(),
            is_error: outcome.is_error,
        };
        let result_payload = tools::tool_result_payload(&call.id, &outcome.content);
        state
            .history
            .append(Role::Tool, vec![result_block], result_payload)
            .await?;
    }

    Ok(true)
}

/// Surfaces a [`ShellClassification`] to the client as a single
/// AssistantText block. Permission-asking is out of scope for now — the
/// classifier's verdict is just informational, displayed in the same
/// stream as model output. The block is not persisted to history: it's
/// a runtime annotation, not part of the model conversation.
async fn emit_classification_block(
    stream: &mut UnixStream,
    send_error: &mut Option<anyhow::Error>,
    id: BlockId,
    classification: &ShellClassification,
) {
    let text = format!(
        "[shell-classify: {}] {}",
        classification.kind.as_str(),
        classification.description,
    );
    try_write(
        stream,
        &StreamFrame::BlockStart {
            id,
            kind: BlockKind::AssistantText,
        },
        send_error,
    )
    .await;
    try_write(stream, &StreamFrame::BlockDelta { id, text }, send_error).await;
    try_write(stream, &StreamFrame::BlockStop { id }, send_error).await;
}

/// Best-effort frame write that records the first send error and silently
/// no-ops afterward. Once the client socket is gone, further frames would
/// just fail; we keep consuming the LLM stream and persisting history so
/// the next attach can replay it.
async fn try_write(
    stream: &mut UnixStream,
    frame: &StreamFrame,
    send_error: &mut Option<anyhow::Error>,
) {
    if send_error.is_some() {
        return;
    }
    if let Err(error) = write_message(stream, frame).await {
        *send_error = Some(anyhow::Error::new(error));
    }
}

fn daemon_status(state: &ServerState) -> DaemonStatus {
    DaemonStatus {
        session_id: SessionId(state.session.id.clone()),
        client_attached: *state
            .client_attached
            .lock()
            .expect("client_attached poisoned"),
        daemon_pid: DaemonPid(state.daemon_pid),
        control_socket_path: state.session.control_socket_path(),
        client_socket_path: state.session.client_socket_path(),
        events_socket_path: state.session.events_socket_path(),
        protocol_version: PROTOCOL_VERSION,
    }
}

/// Builds the layered config provider stack for the daemon. Order is
/// low → high priority — `ConfigHandle::build` applies them in sequence,
/// so later providers override earlier ones.
///
///   1. XDG system config dirs (`XDG_CONFIG_DIRS`, default `/etc/xdg`).
///      Spec orders these most-preferred first; we push in reverse so
///      the most-preferred ends up last among the system layers.
///   2. XDG user config dir (`XDG_CONFIG_HOME`, default `~/.config`).
///   3. `FRANCES__*` env vars.
///   4. Per-session DB rows.
///
/// Each TOML file is `.optional()` — running with no config files
/// present is a supported configuration.
fn build_config_providers(
    session_provider: Arc<SessionConfigProvider>,
) -> Vec<Arc<dyn ConfigProvider>> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix("frances");

    let mut providers: Vec<Arc<dyn ConfigProvider>> = Vec::new();

    for dir in xdg_dirs.get_config_dirs().iter().rev() {
        let path = dir.join("config.toml");
        providers.push(Arc::new(TomlProvider::new(path).optional()));
    }

    if let Some(home) = xdg_dirs.get_config_home() {
        providers.push(Arc::new(
            TomlProvider::new(home.join("config.toml")).optional(),
        ));
    }

    providers.push(Arc::new(EnvProvider::with_prefix("FRANCES")));
    providers.push(session_provider);

    providers
}
