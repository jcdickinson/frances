use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use frances_shell::{
    ReadEvent, RunOpts, RunOutcome, Shell, ShellOptions as LocalShellOptions, WaitOpts,
};
use frances_worker_protocol::{
    Capability, Content, ErrorCode, Feed, FeedSender, FsMetadata, Hello, PROTOCOL_VERSION,
    ProtocolError, Request, RequestKind, Response, ResponseError, ResponseKind, ShellId,
    ShellOptions, ShellOutput, ShellWaitQuiet, multiplex,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

struct ShellResource {
    commands: mpsc::Sender<ShellCommand>,
}

struct ReapableShell {
    shell: Option<Shell>,
    reaper: mpsc::UnboundedSender<Shell>,
}

impl Deref for ReapableShell {
    type Target = Shell;
    fn deref(&self) -> &Shell {
        self.shell.as_ref().expect("shell already reaped")
    }
}

impl DerefMut for ReapableShell {
    fn deref_mut(&mut self) -> &mut Shell {
        self.shell.as_mut().expect("shell already reaped")
    }
}

impl Drop for ReapableShell {
    fn drop(&mut self) {
        if let Some(shell) = self.shell.take() {
            let _ = self.reaper.send(shell);
        }
    }
}

enum ShellCommand {
    Run {
        script: String,
        opts: RunOpts,
        reply: oneshot::Sender<Result<(), ResponseError>>,
    },
    Wait {
        quiet: Duration,
        reply: oneshot::Sender<Result<RunOutcome, ResponseError>>,
    },
    Kill {
        reply: oneshot::Sender<Result<(), ResponseError>>,
    },
    SetVar {
        name: String,
        value: Content,
        reply: oneshot::Sender<Result<(), ResponseError>>,
    },
    GetVar {
        name: String,
        reply: oneshot::Sender<Result<String, ResponseError>>,
    },
}

impl ShellResource {
    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, ResponseError>>) -> ShellCommand,
    ) -> Result<T, ResponseError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| ResponseError::new(ErrorCode::Io, "shell closed"))?;
        response
            .await
            .map_err(|_| ResponseError::new(ErrorCode::Io, "shell closed"))?
    }
}

struct ServerState {
    next_shell: AtomicU64,
    shells: Mutex<HashMap<ShellId, Arc<ShellResource>>>,
    requests: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    shell_reaper: mpsc::UnboundedSender<Shell>,
}

impl ServerState {
    fn new() -> Arc<Self> {
        let (shell_reaper, mut shells) = mpsc::unbounded_channel::<Shell>();
        tokio::spawn(async move {
            while let Some(mut shell) = shells.recv().await {
                shell
                    .shutdown_running(Duration::from_secs(5))
                    .await
                    .inspect_err(|error| tracing::warn!(?error, "failed to kill shell"))
                    .ok();
            }
        });
        Arc::new(Self {
            next_shell: AtomicU64::new(1),
            shells: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            shell_reaper,
        })
    }

    fn abort_all(&self) {
        for (_, request) in self
            .requests
            .lock()
            .expect("request registry poisoned")
            .drain()
        {
            request.abort();
        }
        self.shells.lock().expect("shell registry poisoned").clear();
    }

    fn shell(&self, id: ShellId) -> Result<Arc<ShellResource>, ResponseError> {
        self.shells
            .lock()
            .expect("shell registry poisoned")
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                ResponseError::new(ErrorCode::InvalidRequest, format!("unknown shell {id}"))
            })
    }
}

pub async fn serve<S>(stream: S) -> Result<(), ProtocolError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (mut reader, writer) = multiplex(reader, writer);
    let state = ServerState::new();

    loop {
        let request = match reader.receive::<Request>().await? {
            Some(request) => request,
            None => break,
        };
        let id = request.id;

        if let RequestKind::Cancel { request } = request.kind {
            if let Some(task) = state
                .requests
                .lock()
                .expect("request registry poisoned")
                .remove(&request)
            {
                task.abort();
            }
            writer
                .send(Response {
                    version: PROTOCOL_VERSION,
                    id,
                    result: Ok(ResponseKind::Unit),
                })
                .await?;
            continue;
        }

        if matches!(request.kind, RequestKind::Shutdown) {
            state.abort_all();
            writer
                .send(Response {
                    version: PROTOCOL_VERSION,
                    id,
                    result: Ok(ResponseKind::Unit),
                })
                .await?;
            break;
        }

        let state_for_task = state.clone();
        let writer = writer.clone();
        let (start, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = started.await;
            let result = if request.version != PROTOCOL_VERSION {
                Err(ResponseError::new(
                    ErrorCode::UnsupportedVersion,
                    format!(
                        "protocol version {}, expected {PROTOCOL_VERSION}",
                        request.version
                    ),
                ))
            } else {
                handle(request.kind, &state_for_task).await
            };
            let _ = writer
                .send(Response {
                    version: PROTOCOL_VERSION,
                    id,
                    result,
                })
                .await;
            state_for_task
                .requests
                .lock()
                .expect("request registry poisoned")
                .remove(&id);
        });
        state
            .requests
            .lock()
            .expect("request registry poisoned")
            .insert(id, task.abort_handle());
        let _ = start.send(());
    }

    state.abort_all();
    Ok(())
}

async fn handle(
    kind: RequestKind,
    state: &Arc<ServerState>,
) -> Result<ResponseKind, ResponseError> {
    match kind {
        RequestKind::Hello => Ok(ResponseKind::Hello(Hello {
            version: PROTOCOL_VERSION,
            capabilities: vec![Capability::Filesystem, Capability::Shell],
        })),
        RequestKind::FsRead { path } => {
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Content(Content::from_async_read(file)))
        }
        RequestKind::FsWrite { path, content } => {
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            content
                .copy_to(&mut file)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::FsMetadata { path } => metadata(path).await,
        RequestKind::FsCreateDirAll { path } => {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|error| io_error(&path, error))?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::FsCanonicalize { path } => tokio::fs::canonicalize(&path)
            .await
            .map(ResponseKind::Path)
            .map_err(|error| io_error(&path, error)),
        RequestKind::ShellOpen { options } => open_shell(state, options).await,
        RequestKind::ShellRun {
            shell,
            script,
            stdin,
            persist,
        } => {
            let stdin = read_optional_content(stdin).await.map_err(content_error)?;
            let resource = state.shell(shell)?;
            resource
                .call(|reply| ShellCommand::Run {
                    script,
                    opts: RunOpts { stdin, persist },
                    reply,
                })
                .await?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::ShellWaitQuiet { shell, quiet_ms } => {
            let resource = state.shell(shell)?;
            let outcome = resource
                .call(|reply| ShellCommand::Wait {
                    quiet: Duration::from_millis(quiet_ms),
                    reply,
                })
                .await?;
            match outcome {
                RunOutcome::Quiet { .. } => Ok(ResponseKind::ShellWaitQuiet(ShellWaitQuiet::Quiet)),
                RunOutcome::Done { .. } => Ok(ResponseKind::ShellWaitQuiet(ShellWaitQuiet::Exit)),
                RunOutcome::Dead { .. } => Err(ResponseError::new(
                    ErrorCode::Io,
                    "shell process exited without a status",
                )),
            }
        }
        RequestKind::ShellKill { shell } => {
            state
                .shell(shell)?
                .call(|reply| ShellCommand::Kill { reply })
                .await?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::ShellSetVar { shell, name, value } => {
            state
                .shell(shell)?
                .call(|reply| ShellCommand::SetVar { name, value, reply })
                .await?;
            Ok(ResponseKind::Unit)
        }
        RequestKind::ShellGetVar { shell, name } => {
            let value = state
                .shell(shell)?
                .call(|reply| ShellCommand::GetVar { name, reply })
                .await?;
            Ok(ResponseKind::Content(Content::from_bytes(
                value.into_bytes(),
            )))
        }
        RequestKind::ShellClose { shell } => {
            state
                .shells
                .lock()
                .expect("shell registry poisoned")
                .remove(&shell);
            Ok(ResponseKind::Unit)
        }
        RequestKind::Cancel { .. } | RequestKind::Shutdown => unreachable!(),
    }
}

async fn open_shell(
    state: &Arc<ServerState>,
    options: ShellOptions,
) -> Result<ResponseKind, ResponseError> {
    let shell = Shell::spawn(LocalShellOptions {
        cwd: options.cwd,
        env: options
            .env
            .into_iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        init_script: options.init_script,
    })
    .await
    .map_err(shell_error)?;
    let shell_id = state.next_shell.fetch_add(1, Ordering::Relaxed);
    let (output, output_feed) = Feed::channel();
    let (events, receiver) = mpsc::unbounded_channel();
    let (commands, command_receiver) = mpsc::channel(16);
    tokio::spawn(run_shell(
        ReapableShell {
            shell: Some(shell),
            reaper: state.shell_reaper.clone(),
        },
        output,
        events,
        receiver,
        command_receiver,
    ));
    state
        .shells
        .lock()
        .expect("shell registry poisoned")
        .insert(shell_id, Arc::new(ShellResource { commands }));
    Ok(ResponseKind::ShellOpened {
        shell: shell_id,
        output: output_feed,
    })
}

async fn run_shell(
    mut shell: ReapableShell,
    output: FeedSender<ShellOutput>,
    events: mpsc::UnboundedSender<ReadEvent>,
    mut event_receiver: mpsc::UnboundedReceiver<ReadEvent>,
    mut commands: mpsc::Receiver<ShellCommand>,
) {
    shell.set_output_sink(Some(events));
    loop {
        tokio::select! {
            () = output.closed() => break,
            command = commands.recv() => match command {
                Some(ShellCommand::Run { script, opts, reply }) => {
                    let _ = reply.send(shell.start(&script, opts).await.map_err(shell_error));
                }
                Some(ShellCommand::Wait { quiet, reply }) => {
                    actor_wait(&mut shell, &mut event_receiver, &output, quiet, reply).await;
                    if output.is_closed() { break; }
                }
                Some(ShellCommand::Kill { reply }) => {
                    let result = shell.kill_running().await
                        .inspect_err(|error| tracing::warn!(?error, "failed to kill shell"))
                        .map_err(shell_error);
                    let _ = reply.send(result);
                }
                Some(ShellCommand::SetVar { name, value, reply }) => {
                    let _ = reply.send(set_var(&mut shell, name, value).await);
                }
                Some(ShellCommand::GetVar { name, reply }) => {
                    let _ = reply.send(get_var(&mut shell, name).await);
                }
                None => break,
            }
        }
    }
}

async fn actor_wait(
    shell: &mut Shell,
    events: &mut mpsc::UnboundedReceiver<ReadEvent>,
    output: &FeedSender<ShellOutput>,
    quiet: Duration,
    mut reply: oneshot::Sender<Result<RunOutcome, ResponseError>>,
) {
    let waiting = shell.keep_waiting(WaitOpts {
        quiet: Some(quiet),
        max: None,
    });
    tokio::pin!(waiting);
    let outcome = loop {
        tokio::select! {
            outcome = &mut waiting => break outcome.map_err(shell_error),
            () = output.closed() => return,
            () = reply.closed() => return,
            event = events.recv() => {
                if let Some(event) = event {
                    if !send_output(output, event).await { return; }
                }
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        if !send_output(output, event).await {
            return;
        }
    }
    let _ = reply.send(outcome);
}

async fn send_output(output: &FeedSender<ShellOutput>, event: ReadEvent) -> bool {
    let item = match event {
        ReadEvent::Output(bytes) => Some(ShellOutput::Output {
            content: Content::from_bytes(bytes),
        }),
        ReadEvent::Done { exit_code } => Some(ShellOutput::Exit { exit_code }),
        ReadEvent::Quiet { .. } | ReadEvent::Dead => None,
    };
    if let Some(item) = item {
        output.send(item).await.is_ok()
    } else {
        true
    }
}

async fn set_var(shell: &mut Shell, name: String, value: Content) -> Result<(), ResponseError> {
    validate_bash_name(&name)?;
    if name == "FRANCES_ROOT" {
        return Err(ResponseError::new(
            ErrorCode::InvalidRequest,
            "FRANCES_ROOT is reserved",
        ));
    }
    let value = read_content(value).await.map_err(content_error)?;
    shell.set_output_sink(None);
    let result = shell
        .run_with_opts(
            &format!("export {name}=$(cat)"),
            RunOpts {
                stdin: Some(value),
                persist: vec![name],
            },
            WaitOpts::default(),
        )
        .await;
    match result.map_err(shell_error)? {
        RunOutcome::Done { exit_code: 0, .. } => Ok(()),
        outcome => Err(ResponseError::new(
            ErrorCode::Io,
            format!("set variable: {outcome:?}"),
        )),
    }
}

async fn get_var(shell: &mut Shell, name: String) -> Result<String, ResponseError> {
    validate_bash_name(&name)?;
    shell.set_output_sink(None);
    match shell
        .run(
            &format!("( set -u; printf '%s' \"${name}\" )"),
            WaitOpts::default(),
        )
        .await
        .map_err(shell_error)?
    {
        RunOutcome::Done {
            exit_code: 0,
            output,
        } => Ok(output),
        outcome => Err(ResponseError::new(
            ErrorCode::Io,
            format!("get variable: {outcome:?}"),
        )),
    }
}

fn validate_bash_name(name: &str) -> Result<(), ResponseError> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if valid {
        Ok(())
    } else {
        Err(ResponseError::new(
            ErrorCode::InvalidRequest,
            format!("invalid bash variable name: {name}"),
        ))
    }
}

async fn metadata(path: std::path::PathBuf) -> Result<ResponseKind, ResponseError> {
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| io_error(&path, error))?;
    let modified = metadata
        .modified()
        .map_err(|error| io_error(&path, error))?;
    let duration = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|error| {
            ResponseError::new(
                ErrorCode::Io,
                format!("{}: mtime before Unix epoch: {error}", path.display()),
            )
        })?;
    let mtime_ns = i64::try_from(duration.as_nanos()).map_err(|error| {
        ResponseError::new(
            ErrorCode::Io,
            format!("{}: mtime overflow: {error}", path.display()),
        )
    })?;
    Ok(ResponseKind::Metadata(FsMetadata {
        mtime_ns,
        size: metadata.len(),
        is_dir: metadata.is_dir(),
    }))
}

async fn read_optional_content(content: Option<Content>) -> io::Result<Option<Vec<u8>>> {
    match content {
        Some(content) => read_content(content).await.map(Some),
        None => Ok(None),
    }
}

async fn read_content(content: Content) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    content
        .into_async_read()
        .await?
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

fn shell_error(error: impl std::fmt::Display) -> ResponseError {
    ResponseError::new(ErrorCode::Io, error.to_string())
}

fn content_error(error: io::Error) -> ResponseError {
    ResponseError::new(ErrorCode::Io, format!("content IO: {error}"))
}

fn io_error(path: &std::path::Path, error: io::Error) -> ResponseError {
    ResponseError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}
