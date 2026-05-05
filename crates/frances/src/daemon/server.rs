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

use crate::context::InvocationContext;
use crate::daemon::client::{read_message, remove_socket_if_present, write_message};
use crate::daemon::protocol::{
    AttachResponse, Client, Control, DaemonStatus, PromptId, StreamFrame,
};
use crate::history::{Block, BlockType, HistoryStore, Role};
use crate::llm::{self, InceptionClient};
use crate::session::Session;
use crate::store::Database;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

struct ServerState {
    session: Session,
    client_attached: StdMutex<bool>,
    last_context: StdMutex<Option<InvocationContext>>,
    daemon_pid: u32,
    history: HistoryStore,
    events: EventsRouter,
    shutdown: Notify,
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
struct ControlServer {
    state: Arc<ServerState>,
}

impl Control for ControlServer {
    async fn ping(self, _: context::Context) {}

    async fn status(self, _: context::Context) -> DaemonStatus {
        daemon_status(&self.state)
    }

    async fn stop(self, _: context::Context, _delete_state: bool) {
        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            state.shutdown.notify_waiters();
        });
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
                session_id: self.state.session.id.clone(),
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

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
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

    let state = Arc::new(ServerState {
        session: session.clone(),
        client_attached: StdMutex::new(false),
        last_context: StdMutex::new(None),
        daemon_pid: std::process::id(),
        history: HistoryStore::new(db),
        events: EventsRouter::default(),
        shutdown: Notify::new(),
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

async fn serve_control(path: std::path::PathBuf, state: Arc<ServerState>) -> Result<()> {
    let mut listener = tarpc::serde_transport::unix::listen(&path, Bincode::default).await?;
    listener.config_mut().max_frame_length(usize::MAX);
    while let Some(transport) = listener.next().await {
        let transport = match transport {
            Ok(t) => t,
            Err(error) => {
                warn!(%error, "control accept error");
                continue;
            }
        };
        let server = ControlServer {
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
                    trace!(prompt_id = id, "events socket registered");
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
    let env = state
        .last_context
        .lock()
        .expect("last_context poisoned")
        .as_ref()
        .map(|ctx| ctx.process.env.clone())
        .ok_or_else(|| anyhow::anyhow!("no client context — attach first"))?;

    let llm = InceptionClient::from_env(&env)?;

    let user_block = Block {
        kind: BlockType::Text,
        text: text.clone(),
        data: None,
    };
    let user_payload = serde_json::json!({ "role": "user", "content": text });
    state
        .history
        .append(Role::User, vec![user_block], user_payload)
        .await?;

    let payloads = state.history.openai_payloads().await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let llm_task = tokio::spawn(async move {
        llm.stream(&payloads, move |chunk| {
            let _ = tx.send(chunk.clone());
            Ok(())
        })
        .await
    });

    let mut accumulated = String::new();
    let mut send_error: Option<anyhow::Error> = None;
    let mut chunks: Vec<serde_json::Value> = Vec::new();

    while let Some(chunk) = rx.recv().await {
        for delta in llm::chunk_text_deltas(&chunk) {
            accumulated.push_str(delta);
            if send_error.is_none() {
                if let Err(error) =
                    write_message(stream, &StreamFrame::Text(delta.to_string())).await
                {
                    send_error = Some(anyhow::Error::new(error));
                }
            }
        }
        if let Some(usage) = llm::chunk_usage(&chunk) {
            if send_error.is_none() {
                if let Err(error) = write_message(stream, &StreamFrame::Usage(usage)).await {
                    send_error = Some(anyhow::Error::new(error));
                }
            }
        }
        chunks.push(chunk);
    }

    let stream_result = llm_task.await.context("llm task panicked")?;
    if let Some(error) = send_error {
        return Err(error);
    }
    stream_result?;

    if !accumulated.is_empty() {
        let assistant_block = Block {
            kind: BlockType::Text,
            text: accumulated.clone(),
            data: None,
        };
        let assistant_payload = serde_json::json!({ "role": "assistant", "content": accumulated });
        let assistant_msg = state
            .history
            .append(Role::Assistant, vec![assistant_block], assistant_payload)
            .await?;
        state
            .history
            .append_response_chunks(assistant_msg.id, &chunks)
            .await?;
    }

    Ok(())
}

fn daemon_status(state: &ServerState) -> DaemonStatus {
    DaemonStatus {
        session_id: state.session.id.clone(),
        client_attached: *state
            .client_attached
            .lock()
            .expect("client_attached poisoned"),
        daemon_pid: state.daemon_pid,
        control_socket_path: state.session.control_socket_path(),
        client_socket_path: state.session.client_socket_path(),
        events_socket_path: state.session.events_socket_path(),
        protocol_version: 1,
    }
}
