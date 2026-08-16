use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use frances_shell::{QuietReason, ReadEvent, RunOpts, RunOutcome, WaitOpts};
use frances_worker_protocol::{
    Capability, Content, Feed, FsMetadata, PROTOCOL_VERSION, ProtocolError, ProtocolReader,
    ProtocolWriter, Request, RequestKind, Response, ResponseKind, ShellId, ShellOptions,
    ShellOutput, ShellWaitQuiet, multiplex,
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
        match self.call(RequestKind::ShellOpen { options }).await? {
            ResponseKind::ShellOpened { shell, output } => Ok(WorkerShell {
                id: shell,
                client: self.clone(),
                output,
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
        let mut cancel = CancelOnDrop {
            client: self.clone(),
            request: id,
            armed: true,
        };
        let response = response.await.map_err(|_| ClientError::Closed)?;
        cancel.armed = false;
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

struct CancelOnDrop {
    client: Client,
    request: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.client
            .inner
            .pending
            .lock()
            .expect("worker response map poisoned")
            .remove(&self.request);
        let id = self.client.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let writer = self.client.inner.writer.clone();
        let request = self.request;
        tokio::spawn(async move {
            let _ = writer
                .send(Request {
                    version: PROTOCOL_VERSION,
                    id,
                    kind: RequestKind::Cancel { request },
                })
                .await;
        });
    }
}

pub struct WorkerShell {
    id: ShellId,
    client: Client,
    output: Feed<ShellOutput>,
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
        self.client
            .expect_unit(RequestKind::ShellRun {
                shell: self.id,
                script: script.to_owned(),
                stdin: options.stdin.map(Content::from_bytes),
                persist: options.persist,
            })
            .await?;
        self.wait(wait).await
    }

    pub async fn keep_waiting(&mut self, wait: WaitOpts) -> Result<RunOutcome, ClientError> {
        self.wait(wait).await
    }

    pub async fn kill_running(&mut self) -> Result<(), ClientError> {
        self.client
            .expect_unit(RequestKind::ShellKill { shell: self.id })
            .await
    }

    pub async fn set_var(&mut self, name: String, value: Content) -> Result<(), ClientError> {
        self.client
            .expect_unit(RequestKind::ShellSetVar {
                shell: self.id,
                name,
                value,
            })
            .await
    }

    pub async fn get_var(&mut self, name: String) -> Result<Content, ClientError> {
        match self
            .client
            .call(RequestKind::ShellGetVar {
                shell: self.id,
                name,
            })
            .await?
        {
            ResponseKind::Content(content) => Ok(content),
            _ => Err(ClientError::WrongResponseKind),
        }
    }

    async fn wait(&mut self, wait: WaitOpts) -> Result<RunOutcome, ClientError> {
        let quiet = wait.quiet.unwrap_or(frances_shell::DEFAULT_QUIET);
        let waiting = self.wait_quiet(quiet);
        if let Some(max) = wait.max {
            match tokio::time::timeout(max, waiting).await {
                Ok(result) => self.read_until(result?).await,
                Err(_) => Ok(RunOutcome::Quiet {
                    output: String::new(),
                    reason: QuietReason::MaxElapsed,
                }),
            }
        } else {
            self.read_until(waiting.await?).await
        }
    }

    /// Wait for the shell to become quiet or exit.
    ///
    /// This is a protocol operation of its own so callers can wrap it in a
    /// timeout. Dropping the future sends a cancellation request to the
    /// worker, which aborts the corresponding server task.
    pub async fn wait_quiet(
        &self,
        quiet: std::time::Duration,
    ) -> Result<ShellWaitQuiet, ClientError> {
        let result = self
            .client
            .call(RequestKind::ShellWaitQuiet {
                shell: self.id,
                quiet_ms: duration_millis(quiet),
            })
            .await?;
        let wait = match result {
            ResponseKind::ShellWaitQuiet(wait) => wait,
            _ => return Err(ClientError::WrongResponseKind),
        };
        Ok(wait)
    }

    async fn read_until(&mut self, wait: ShellWaitQuiet) -> Result<RunOutcome, ClientError> {
        let mut output = Vec::new();
        if matches!(wait, ShellWaitQuiet::Quiet) {
            while let Some(item) = self.output.try_next()? {
                let ShellOutput::Output { content } = item else {
                    return Err(ClientError::WrongResponseKind);
                };
                let bytes = read_content(content).await?;
                output.extend_from_slice(&bytes);
                if let Some(sink) = &self.output_sink {
                    let _ = sink.send(ReadEvent::Output(bytes));
                }
            }
            let reason = QuietReason::NoOutput;
            self.send_read_event(ReadEvent::Quiet { reason });
            return Ok(RunOutcome::Quiet {
                output: String::from_utf8_lossy(&output).into_owned(),
                reason,
            });
        }
        loop {
            match self.output.next().await?.ok_or(ClientError::Closed)? {
                ShellOutput::Output { content } => {
                    let bytes = read_content(content).await?;
                    output.extend_from_slice(&bytes);
                    if let Some(sink) = &self.output_sink {
                        let _ = sink.send(ReadEvent::Output(bytes));
                    }
                }
                ShellOutput::Exit { exit_code } => {
                    self.send_read_event(ReadEvent::Done { exit_code });
                    return Ok(RunOutcome::Done {
                        exit_code,
                        output: String::from_utf8_lossy(&output).into_owned(),
                    });
                }
            }
        }
    }

    fn send_read_event(&self, event: ReadEvent) {
        if let Some(sink) = &self.output_sink {
            let _ = sink.send(event);
        }
    }
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
