use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures::StreamExt;
use tarpc::context;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, oneshot};
use tracing::{debug, info, trace, warn};

use crate::Result;
use crate::anchor_store::AnchorStoreImpl;
use crate::chat::{ChatSession, ChatSessionBuilder, ChatSessionManager};
use crate::context::InvocationContext;
use crate::edit_session::EditSession;
use crate::history::HistoryStore;
use crate::llm::provider_cache::ProviderCache;
use crate::llm::{ModelConfig, SessionConfigProvider, SessionConfigWriter, StreamEvent};
use crate::protocol::{
    AttachResponse, BlockId, BlockKind, Client, DaemonPid, DaemonStatus, PROTOCOL_VERSION,
    PromptId, SessionId, StreamFrame,
};
use crate::session::Session;
use crate::shell_classifier::{self, ShellClassification};
use crate::store::Database;
use crate::tools::{self, ToolRegistry};
use crate::transport::{TransportError, read_message, remove_socket_if_present, write_message};
use crate::workflows::{self, WorkflowConfig};
use frances_config::{ConfigBinding, ConfigHandle, ConfigProvider, EnvProvider, TomlProvider};
use frances_edit::EditEngine;
use frances_shell::Shell;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("no client context — attach first")]
    NoClientContext,
    #[error("open daemon log {path}: {source}")]
    OpenDaemonLog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("dup2 stdout to daemon log: {0}")]
    Dup2Stdout(#[source] io::Error),
    #[error("dup2 stderr to daemon log: {0}")]
    Dup2Stderr(#[source] io::Error),
    #[error("install tracing subscriber: {0}")]
    InstallSubscriber(#[from] tracing_subscriber::util::TryInitError),
    #[error("create runtime dir {path}: {source}")]
    CreateRuntimeDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("write pid file for {session_id}: {source}")]
    WritePidFile {
        session_id: String,
        #[source]
        source: io::Error,
    },
    #[error("bind {label} socket {path}: {source}")]
    BindSocket {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("models::default is required")]
    DefaultModelMissing,
    #[error("llm task panicked: {0}")]
    LlmTaskPanicked(#[from] tokio::task::JoinError),
    #[error("send stream frame: {0}")]
    Send(#[from] TransportError),
    #[error("clean up {label} socket {path}: {source}")]
    CleanupSocket {
        label: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("control protocol I/O: {0}")]
    ControlIo(#[source] io::Error),
    #[error("client transport listen: {0}")]
    ClientListen(#[source] io::Error),
}

pub(crate) struct ServerState {
    session: Session,
    client_attached: StdMutex<bool>,
    last_context: StdMutex<Option<InvocationContext>>,
    daemon_pid: u32,
    edit_session: tokio::sync::Mutex<EditSession<AnchorStoreImpl>>,
    shell: tokio::sync::Mutex<Option<Shell>>,
    tool_registry: ToolRegistry,
    events: EventsRouter,
    shutdown: Notify,
    /// Kept alive so the config-event-processor task stays running for the
    /// daemon's lifetime. The chat manager and provider cache hold their
    /// own bindings, but parking the handle here makes the lifetime
    /// guarantee explicit.
    #[expect(dead_code, reason = "lifetime anchor for the config event processor")]
    config: ConfigHandle,
    chat: Arc<ChatSessionManager>,
    /// The session driving the TUI's hardcoded turn workflow. There's
    /// only one for now; loaded (or created) once at daemon startup.
    primary_chat: Arc<ChatSession>,
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
    ) -> std::result::Result<(), String> {
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
        .map_err(|source| ServerError::OpenDaemonLog {
            path: log_path.clone(),
            source,
        })?;

    let fd = file.as_raw_fd();
    unsafe {
        if libc::dup2(fd, libc::STDOUT_FILENO) < 0 {
            return Err(ServerError::Dup2Stdout(io::Error::last_os_error()).into());
        }
        if libc::dup2(fd, libc::STDERR_FILENO) < 0 {
            return Err(ServerError::Dup2Stderr(io::Error::last_os_error()).into());
        }
    }
    drop(file);

    // Default to warn for the world; raise frances/frances-edit/frances-anchors
    // /frances-config to trace so we can see our own logs without drowning in
    // turso/hyper/reqwest internals. Overridable via RUST_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "warn,frances=trace,frances_daemon=trace,frances_edit=trace,frances_anchors=trace,frances_config=trace",
        )
    });
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(io::stderr)
        .finish()
        .try_init()
        .map_err(ServerError::InstallSubscriber)?;

    info!(session_id = %session.id, log = %log_path.display(), "daemon logging installed");
    Ok(())
}

pub async fn run(session: Session, db: Database) -> Result<()> {
    debug!(session_id = %session.id, "starting daemon server");

    fs::create_dir_all(&session.runtime_dir).map_err(|source| ServerError::CreateRuntimeDir {
        path: session.runtime_dir.clone(),
        source,
    })?;

    remove_socket_if_present(&session.control_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "control",
            path: session.control_socket_path(),
            source,
        }
    })?;
    remove_socket_if_present(&session.client_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "client",
            path: session.client_socket_path(),
            source,
        }
    })?;
    remove_socket_if_present(&session.events_socket_path()).map_err(|source| {
        ServerError::CleanupSocket {
            label: "events",
            path: session.events_socket_path(),
            source,
        }
    })?;

    fs::write(session.pid_path(), std::process::id().to_string()).map_err(|source| {
        ServerError::WritePidFile {
            session_id: session.id.clone(),
            source,
        }
    })?;

    let edit_engine = EditEngine::new(AnchorStoreImpl::new(db.clone()));

    let session_provider = Arc::new(SessionConfigProvider::new(db.clone()));
    let config_providers = build_config_providers(session_provider.clone());
    let config = ConfigHandle::build(config_providers).await?;
    let session_config_writer = session_provider
        .writer()
        .expect("SessionConfigProvider::load ran during ConfigHandle::build");
    let default_model = config
        .bind::<ModelConfig>(["models", "default"])?
        .required()
        .map_err(|_| ServerError::DefaultModelMissing)?;
    let provider_cache = Arc::new(ProviderCache::new(config.clone())?);
    let workflows = config.bind::<HashMap<String, WorkflowConfig>>("workflows")?;

    let history = HistoryStore::new(db);
    let chat = ChatSessionManager::new(provider_cache, config.clone(), default_model, history)?;
    let primary_chat = chat
        .primary(ChatSessionBuilder::new().with_model_intents(["chat".to_string()]))
        .await?;

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: StdMutex::new(false),
        last_context: StdMutex::new(None),
        daemon_pid: std::process::id(),
        edit_session: tokio::sync::Mutex::new(EditSession::new(edit_engine)),
        shell: tokio::sync::Mutex::new(None),
        tool_registry: ToolRegistry::builtin(),
        events: EventsRouter::default(),
        shutdown: Notify::new(),
        config,
        chat,
        primary_chat,
        workflows,
        session_config_writer,
    });

    let events_listener = UnixListener::bind(session.events_socket_path()).map_err(|source| {
        ServerError::BindSocket {
            label: "events",
            path: session.events_socket_path(),
            source,
        }
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
async fn serve_control(
    path: PathBuf,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    let listener = UnixListener::bind(&path).map_err(|source| ServerError::BindSocket {
        label: "control",
        path: path.clone(),
        source,
    })?;
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

async fn handle_control_conn(
    stream: &mut UnixStream,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.split();

    // Greet with our build's protocol id so the client can decide compatibility
    // before issuing any command.
    write_half
        .write_all(format!("{PROTOCOL_VERSION:016x}\n").as_bytes())
        .await
        .map_err(ServerError::ControlIo)?;
    write_half.flush().await.map_err(ServerError::ControlIo)?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .map_err(ServerError::ControlIo)?
        == 0
    {
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

    write_half
        .write_all(response.as_bytes())
        .await
        .map_err(ServerError::ControlIo)?;
    write_half.flush().await.map_err(ServerError::ControlIo)?;
    Ok(())
}

async fn serve_client(
    path: PathBuf,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    let mut listener = tarpc::serde_transport::unix::listen(&path, Bincode::default)
        .await
        .map_err(ServerError::ClientListen)?;
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
        match write_message(&mut stream, &StreamFrame::Error(format!("{error}"))).await {
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
        let ctx = guard.as_ref().ok_or(ServerError::NoClientContext)?;
        (ctx.process.env.clone(), ctx.process.cwd.clone())
    };

    let chat = state.primary_chat.clone();

    let mut next_block: u64 = 1;
    let mut alloc_block = || {
        let id = BlockId(next_block);
        next_block += 1;
        id
    };

    let mut send_error: Option<TransportError> = None;

    chat.submit_user(&text).await?;

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
            &chat,
            &env,
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
        return Err(ServerError::Send(error).into());
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
    chat: &Arc<ChatSession>,
    env: &HashMap<OsString, OsString>,
    alloc_block: &mut impl FnMut() -> BlockId,
    send_error: &mut Option<TransportError>,
    cwd: Option<&std::path::Path>,
) -> Result<bool> {
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

    let chat_for_task = chat.clone();
    let env_for_task = env.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
    let llm_task = tokio::spawn(async move {
        chat_for_task
            .run(
                &env_for_task,
                &tool_defs,
                None,
                move |event: StreamEvent| {
                    let _ = tx.send(event);
                    Ok(())
                },
            )
            .await
    });

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                if wire_active == Some(assistant_id) {
                    try_write(
                        stream,
                        &StreamFrame::BlockDelta {
                            id: assistant_id,
                            text: delta,
                        },
                        send_error,
                    )
                    .await;
                }
            }
            StreamEvent::ToolCall(call) => {
                if let Some(active) = wire_active.take() {
                    try_write(stream, &StreamFrame::BlockStop { id: active }, send_error).await;
                }
                let block_id = alloc_block();
                wire_active = Some(block_id);
                try_write(
                    stream,
                    &StreamFrame::BlockStart {
                        id: block_id,
                        kind: BlockKind::ToolUse {
                            name: call.name.clone(),
                        },
                    },
                    send_error,
                )
                .await;
                try_write(
                    stream,
                    &StreamFrame::BlockDelta {
                        id: block_id,
                        text: call.arguments.to_string(),
                    },
                    send_error,
                )
                .await;
            }
            StreamEvent::Usage(usage) => {
                try_write(stream, &StreamFrame::Usage(usage), send_error).await;
            }
            StreamEvent::History(_) => {
                // ChatSession::run consumes History events internally; this
                // arm exists only to keep the match exhaustive.
            }
        }
    }

    if let Some(id) = wire_active.take() {
        try_write(stream, &StreamFrame::BlockStop { id }, send_error).await;
    }

    let stream_result = llm_task.await.map_err(ServerError::LlmTaskPanicked)?;
    let outcome = stream_result?;
    let tool_calls = outcome.tool_calls;

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
            let classification =
                shell_classifier::classify_shell(&state.chat, chat.session_id(), env, cmd).await;
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

        chat.submit_tool_result(&call.id, &outcome.content, outcome.is_error)
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
    send_error: &mut Option<TransportError>,
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
    send_error: &mut Option<TransportError>,
) {
    if send_error.is_some() {
        return;
    }
    if let Err(error) = write_message(stream, frame).await {
        *send_error = Some(error);
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
