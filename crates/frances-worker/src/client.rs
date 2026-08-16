use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use frances_shell::{QuietReason, ReadEvent, RunOpts, RunOutcome, WaitOpts};
use frances_worker_protocol::{
    Capability, Content, Feed, FeedSender, FsMetadata, PROTOCOL_VERSION, ProtocolError,
    ProtocolReader, ProtocolWriter, Request, RequestKind, Response, ResponseKind, ShellCommand,
    ShellEvent, ShellEventKind, ShellId, ShellOperationId, ShellOptions, ShellQuietReason,
    ShellWait, multiplex,
};
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    writer: ProtocolWriter,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>,
    _child: Mutex<Child>,
    next_id: AtomicU64,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("resolve local worker executable: {0}")]
    ResolveExecutable(#[source] io::Error),
    #[error("local worker executable has no parent directory")]
    MissingExecutableParent,
    #[error("start local worker {}: {source}", path.display())]
    Spawn { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("content IO: {0}")]
    ContentIo(#[from] io::Error),
    #[error("worker closed the connection")]
    Closed,
    #[error("worker replied to request {actual}, expected {expected}")]
    WrongResponseId { expected: u64, actual: u64 },
    #[error("worker protocol version {actual}, expected {expected}")]
    Version { expected: u32, actual: u32 },
    #[error("worker does not advertise the filesystem capability")]
    MissingFilesystemCapability,
    #[error("worker does not advertise the shell capability")]
    MissingShellCapability,
    #[error("worker error: {0}")]
    Worker(String),
    #[error("worker returned the wrong response kind")]
    WrongResponseKind,
}

impl Client {
    /// Spawn the sibling worker binary. Transport selection is deliberately
    /// hardcoded for milestone one; SSH and WSL will provide the same stdio
    /// shape later.
    pub async fn spawn_local() -> Result<Self, ClientError> {
        let executable = std::env::current_exe().map_err(ClientError::ResolveExecutable)?;
        let parent = executable
            .parent()
            .ok_or(ClientError::MissingExecutableParent)?;
        let worker_name = if cfg!(windows) {
            "frances-worker.exe"
        } else {
            "frances-worker"
        };
        let worker_path = parent.join(worker_name);

        let mut child = Command::new(&worker_path)
            .arg("serve")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ClientError::Spawn {
                path: worker_path,
                source,
            })?;
        let stdin = child.stdin.take().expect("piped worker stdin");
        let stdout = child.stdout.take().expect("piped worker stdout");
        let (reader, writer) = multiplex(stdout, stdin);
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        tokio::spawn(route_responses(reader, pending.clone()));
        let client = Self {
            inner: Arc::new(Inner {
                writer,
                pending,
                _child: Mutex::new(child),
                next_id: AtomicU64::new(1),
            }),
        };

        match client.call(RequestKind::Hello).await? {
            ResponseKind::Hello(hello) => {
                if hello.version != PROTOCOL_VERSION {
                    return Err(ClientError::Version {
                        expected: PROTOCOL_VERSION,
                        actual: hello.version,
                    });
                }
                if !hello.capabilities.contains(&Capability::Filesystem) {
                    return Err(ClientError::MissingFilesystemCapability);
                }
                if !hello.capabilities.contains(&Capability::Shell) {
                    return Err(ClientError::MissingShellCapability);
                }
            }
            _ => return Err(ClientError::WrongResponseKind),
        }
        Ok(client)
    }

    pub async fn read(&self, path: &Path) -> Result<Content, ClientError> {
        match self
            .call(RequestKind::FsRead {
                path: path.to_path_buf(),
            })
            .await?
        {
            ResponseKind::Content(content) => Ok(content),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    pub async fn write(&self, path: &Path, content: Content) -> Result<(), ClientError> {
        self.expect_unit(RequestKind::FsWrite {
            path: path.to_path_buf(),
            content,
        })
        .await
    }

    pub async fn metadata(&self, path: &Path) -> Result<FsMetadata, ClientError> {
        match self
            .call(RequestKind::FsMetadata {
                path: path.to_path_buf(),
            })
            .await?
        {
            ResponseKind::Metadata(metadata) => Ok(metadata),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    pub async fn create_dir_all(&self, path: &Path) -> Result<(), ClientError> {
        self.expect_unit(RequestKind::FsCreateDirAll {
            path: path.to_path_buf(),
        })
        .await
    }

    pub async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ClientError> {
        match self
            .call(RequestKind::FsCanonicalize {
                path: path.to_path_buf(),
            })
            .await?
        {
            ResponseKind::Path(path) => Ok(path),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    pub async fn open_shell(&self, options: ShellOptions) -> Result<WorkerShell, ClientError> {
        let (commands, command_feed) = Feed::channel();
        match self
            .call(RequestKind::ShellOpen {
                options,
                commands: command_feed,
            })
            .await?
        {
            ResponseKind::ShellOpened { shell, events } => Ok(WorkerShell {
                id: shell,
                commands,
                events,
                next_operation: 1,
                output_sink: None,
            }),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    async fn expect_unit(&self, kind: RequestKind) -> Result<(), ClientError> {
        match self.call(kind).await? {
            ResponseKind::Unit => Ok(()),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    async fn call(&self, kind: RequestKind) -> Result<ResponseKind, ClientError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            version: PROTOCOL_VERSION,
            id,
            kind,
        };
        let (reply, response) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("worker response map poisoned")
            .insert(id, reply);
        if let Err(error) = self.inner.writer.send(request).await {
            self.inner
                .pending
                .lock()
                .expect("worker response map poisoned")
                .remove(&id);
            return Err(error.into());
        }
        let response = response.await.map_err(|_| ClientError::Closed)?;
        if response.id != id {
            return Err(ClientError::WrongResponseId {
                expected: id,
                actual: response.id,
            });
        }
        if response.version != PROTOCOL_VERSION {
            return Err(ClientError::Version {
                expected: PROTOCOL_VERSION,
                actual: response.version,
            });
        }
        response
            .result
            .map_err(|error| ClientError::Worker(error.error.message))
    }
}

pub struct WorkerShell {
    id: ShellId,
    commands: FeedSender<ShellCommand>,
    events: Feed<ShellEvent>,
    next_operation: ShellOperationId,
    output_sink: Option<tokio::sync::mpsc::UnboundedSender<ReadEvent>>,
}

impl WorkerShell {
    pub fn id(&self) -> ShellId {
        self.id
    }

    pub fn set_output_sink(&mut self, sink: Option<tokio::sync::mpsc::UnboundedSender<ReadEvent>>) {
        self.output_sink = sink;
    }

    pub async fn run_with_opts(
        &mut self,
        script: &str,
        options: RunOpts,
        wait: WaitOpts,
    ) -> Result<RunOutcome, ClientError> {
        let operation = self.operation();
        self.commands
            .send(ShellCommand::Run {
                operation,
                script: script.to_owned(),
                stdin: options.stdin.map(Content::from_bytes),
                persist: options.persist,
                wait: wire_wait(wait),
            })
            .await
            .map_err(|_| ClientError::Closed)?;
        self.observe(operation).await
    }

    pub async fn keep_waiting(&mut self, wait: WaitOpts) -> Result<RunOutcome, ClientError> {
        let operation = self.operation();
        self.commands
            .send(ShellCommand::KeepWaiting {
                operation,
                wait: wire_wait(wait),
            })
            .await
            .map_err(|_| ClientError::Closed)?;
        self.observe(operation).await
    }

    pub async fn kill_running(&mut self) -> Result<(), ClientError> {
        let operation = self.operation();
        self.commands
            .send(ShellCommand::Kill { operation })
            .await
            .map_err(|_| ClientError::Closed)?;
        self.expect_ack(operation).await
    }

    pub async fn set_var(&mut self, name: String, value: Content) -> Result<(), ClientError> {
        let operation = self.operation();
        self.commands
            .send(ShellCommand::SetVar {
                operation,
                name,
                value,
            })
            .await
            .map_err(|_| ClientError::Closed)?;
        self.expect_ack(operation).await
    }

    pub async fn get_var(&mut self, name: String) -> Result<Content, ClientError> {
        let operation = self.operation();
        self.commands
            .send(ShellCommand::GetVar { operation, name })
            .await
            .map_err(|_| ClientError::Closed)?;
        let event = self.next_for(operation).await?;
        match event.kind {
            ShellEventKind::Value { content } => Ok(content),
            ShellEventKind::Error { message } => Err(ClientError::Worker(message)),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    fn operation(&mut self) -> ShellOperationId {
        let operation = self.next_operation;
        self.next_operation += 1;
        operation
    }

    async fn observe(&mut self, operation: ShellOperationId) -> Result<RunOutcome, ClientError> {
        let mut output = Vec::new();
        loop {
            let event = self.next_for(operation).await?;
            match event.kind {
                ShellEventKind::Output { content } => {
                    let bytes = read_content(content).await?;
                    output.extend_from_slice(&bytes);
                    if let Some(sink) = &self.output_sink {
                        let _ = sink.send(ReadEvent::Output(bytes));
                    }
                }
                ShellEventKind::Done { exit_code } => {
                    self.send_read_event(ReadEvent::Done { exit_code });
                    return Ok(RunOutcome::Done {
                        exit_code,
                        output: String::from_utf8_lossy(&output).into_owned(),
                    });
                }
                ShellEventKind::Quiet { reason } => {
                    let reason = local_quiet_reason(reason);
                    self.send_read_event(ReadEvent::Quiet { reason });
                    return Ok(RunOutcome::Quiet {
                        output: String::from_utf8_lossy(&output).into_owned(),
                        reason,
                    });
                }
                ShellEventKind::Dead => {
                    self.send_read_event(ReadEvent::Dead);
                    return Ok(RunOutcome::Dead {
                        output: String::from_utf8_lossy(&output).into_owned(),
                    });
                }
                ShellEventKind::Error { message } => return Err(ClientError::Worker(message)),
                _ => return Err(ClientError::WrongResponseKind),
            }
        }
    }

    async fn expect_ack(&mut self, operation: ShellOperationId) -> Result<(), ClientError> {
        match self.next_for(operation).await?.kind {
            ShellEventKind::Ack => Ok(()),
            ShellEventKind::Error { message } => Err(ClientError::Worker(message)),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    async fn next_for(&mut self, operation: ShellOperationId) -> Result<ShellEvent, ClientError> {
        let event = self.events.next().await?.ok_or(ClientError::Closed)?;
        if event.operation != operation {
            return Err(ClientError::Worker(format!(
                "shell {} received operation {}, expected {operation}",
                self.id, event.operation
            )));
        }
        Ok(event)
    }

    fn send_read_event(&self, event: ReadEvent) {
        if let Some(sink) = &self.output_sink {
            let _ = sink.send(event);
        }
    }
}

fn wire_wait(wait: WaitOpts) -> ShellWait {
    ShellWait {
        quiet_ms: wait.quiet.map(duration_millis),
        max_ms: wait.max.map(duration_millis),
    }
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn local_quiet_reason(reason: ShellQuietReason) -> QuietReason {
    match reason {
        ShellQuietReason::NoOutput => QuietReason::NoOutput,
        ShellQuietReason::MaxElapsed => QuietReason::MaxElapsed,
    }
}

async fn read_content(content: Content) -> Result<Vec<u8>, ClientError> {
    use tokio::io::AsyncReadExt;

    let mut bytes = Vec::new();
    content
        .into_async_read()
        .await?
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

async fn route_responses(
    mut reader: ProtocolReader,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>,
) {
    while let Ok(Some(response)) = reader.receive::<Response>().await {
        if let Some(reply) = pending
            .lock()
            .expect("worker response map poisoned")
            .remove(&response.id)
        {
            let _ = reply.send(response);
        }
    }
    pending
        .lock()
        .expect("worker response map poisoned")
        .clear();
}
