use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use frances_shell::{QuietReason, ReadEvent, RunOpts, RunOutcome, WaitOpts};
use frances_worker_protocol::{
    Capability, Content, ErrorCode, Feed, FileSearchEvent, FileSearchOptions, FsMetadata,
    FsWriteMode, PROTOCOL_VERSION, ProtocolError, ProtocolFeedError, ProtocolReader,
    ProtocolWriter, Request, RequestKind, Response, ResponseKind, ShellId, ShellOptions,
    ShellOutput, ShellWaitQuiet, multiplex,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    writer: ProtocolWriter,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>,
    _child: Option<Mutex<Child>>,
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
    #[error(transparent)]
    Feed(#[from] ProtocolFeedError),
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
    #[error("worker does not advertise the process capability")]
    MissingProcessCapability,
    #[error("worker error: {message}")]
    Worker { code: ErrorCode, message: String },
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

        Self::spawn(worker_path).await
    }

    /// Spawn a worker binary at an explicitly resolved path.
    pub async fn spawn(worker_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let worker_path = worker_path.as_ref().to_path_buf();
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
        Self::connect_io(stdout, stdin, Some(child)).await
    }

    /// Connect to a worker over an existing bidirectional transport.
    pub async fn connect<S>(stream: S) -> Result<Self, ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        Self::connect_io(reader, writer, None).await
    }

    async fn connect_io<R, W>(
        reader: R,
        writer: W,
        child: Option<Child>,
    ) -> Result<Self, ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = multiplex(reader, writer);
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        tokio::spawn(route_responses(reader, pending.clone()));
        let client = Self {
            inner: Arc::new(Inner {
                writer,
                pending,
                _child: child.map(Mutex::new),
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
                if !hello.capabilities.contains(&Capability::Process) {
                    return Err(ClientError::MissingProcessCapability);
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
            mode: FsWriteMode::Overwrite,
        })
        .await
    }

    pub async fn write_create_new(&self, path: &Path, content: Content) -> Result<(), ClientError> {
        self.expect_unit(RequestKind::FsWrite {
            path: path.to_path_buf(),
            content,
            mode: FsWriteMode::CreateNew,
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

    pub async fn find_or_grep(
        &self,
        options: FileSearchOptions,
    ) -> Result<Feed<FileSearchEvent>, ClientError> {
        let ResponseKind::FileSearch { results } =
            self.call(RequestKind::FsFindOrGrep { options }).await?
        else {
            return Err(ClientError::WrongResponseKind);
        };
        Ok(results)
    }

    pub async fn shutdown(&self) -> Result<(), ClientError> {
        self.expect_unit(RequestKind::Shutdown).await
    }

    pub async fn open_process(
        &self,
        options: frances_worker_protocol::ProcessOptions,
    ) -> Result<tokio::io::DuplexStream, ClientError> {
        let (input, receiver) = Feed::channel();
        let ResponseKind::ProcessOpened { mut output } = self
            .call(RequestKind::ProcessOpen {
                options,
                input: receiver,
            })
            .await?
        else {
            return Err(ClientError::WrongResponseKind);
        };
        let (stream, bridge) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut reader, mut writer) = tokio::io::split(bridge);
            let send = async {
                let mut bytes = vec![0; 16 * 1024];
                loop {
                    let count = reader.read(&mut bytes).await?;
                    if count == 0 {
                        return Ok::<_, io::Error>(());
                    }
                    input
                        .send(Content::from_bytes(bytes[..count].to_vec()))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "worker process input closed")
                        })?;
                }
            };
            let receive = async {
                while let Some(content) = output.next().await.map_err(io::Error::other)? {
                    content.copy_to(&mut writer).await?;
                }
                Ok::<_, io::Error>(())
            };
            let result = tokio::select! { result = send => result, result = receive => result };
            if let Err(error) = result {
                tracing::debug!(%error, "worker process stream closed");
            }
        });
        Ok(stream)
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
        response.result.map_err(|error| ClientError::Worker {
            code: error.error.code,
            message: error.error.message,
        })
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
        let client = self.client.clone();
        let id = self.id;
        let quiet = wait.quiet.unwrap_or(frances_shell::DEFAULT_QUIET);
        let waiting = async move {
            match client
                .call(RequestKind::ShellWaitQuiet {
                    shell: id,
                    quiet_ms: duration_millis(quiet),
                })
                .await?
            {
                ResponseKind::ShellWaitQuiet(wait) => Ok(wait),
                _ => Err(ClientError::WrongResponseKind),
            }
        };
        tokio::pin!(waiting);
        let deadline = async {
            match wait.max {
                Some(max) => tokio::time::sleep(max).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        let mut output = Vec::new();
        let reason = loop {
            tokio::select! {
                biased;
                () = &mut deadline => break QuietReason::MaxElapsed,
                item = self.output.next() => {
                    if let Some(outcome) = self.collect_output(item?.ok_or(ClientError::Closed)?, &mut output).await? {
                        return Ok(outcome);
                    }
                }
                status = &mut waiting => {
                    match status? {
                        ShellWaitQuiet::Quiet => break QuietReason::NoOutput,
                        ShellWaitQuiet::Exit => {
                            // The matching exit marker follows the command's
                            // bytes on the feed. Drain through that marker.
                            loop {
                                let item = self.output.next().await?.ok_or(ClientError::Closed)?;
                                if let Some(outcome) = self.collect_output(item, &mut output).await? { return Ok(outcome); }
                            }
                        }
                    }
                }
            }
        };
        while let Some(item) = self.output.try_next()? {
            if let Some(outcome) = self.collect_output(item, &mut output).await? {
                return Ok(outcome);
            }
        }
        self.send_read_event(ReadEvent::Quiet { reason });
        Ok(RunOutcome::Quiet {
            output: String::from_utf8_lossy(&output).into_owned(),
            reason,
        })
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

    async fn collect_output(
        &self,
        item: ShellOutput,
        output: &mut Vec<u8>,
    ) -> Result<Option<RunOutcome>, ClientError> {
        match item {
            ShellOutput::Output { content } => {
                let bytes = read_content(content).await?;
                output.extend_from_slice(&bytes);
                self.send_read_event(ReadEvent::Output(bytes));
                Ok(None)
            }
            ShellOutput::Exit { exit_code } => {
                self.send_read_event(ReadEvent::Done { exit_code });
                Ok(Some(RunOutcome::Done {
                    exit_code,
                    output: String::from_utf8_lossy(output).into_owned(),
                }))
            }
        }
    }

    fn send_read_event(&self, event: ReadEvent) {
        if let Some(sink) = &self.output_sink
            && let Err(error) = sink.send(event)
        {
            tracing::debug!(%error, "shell output receiver closed");
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
