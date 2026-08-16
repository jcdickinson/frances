//! Production IO impls. `RealIo` is what the session runtime hands to
//! `WorkflowDepsImpl`; tests drag in [`super::mock::MockIo`] instead.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use frances_shell::{ReadEvent, RunOpts, RunOutcome, Shell, ShellError, ShellOptions, WaitOpts};
use frances_worker::{Client as WorkerClient, WorkerShell};
use frances_worker_protocol::{
    Content, ErrorCode, FileSearchEvent, FileSearchOptions, ShellOptions as WorkerShellOptions,
};

use super::{
    FileSearchCollector, FileSearchResults, FsMetadata, SleepOutcome, WorkflowFs, WorkflowIo,
    WorkflowShell, WorkflowShellHandle, WorkflowTimer,
};
use crate::closed::WorkflowClosed;

/// Production IO bundle.
#[derive(Clone, Copy, Default)]
pub struct RealIo {
    timer: RealTimer,
    shell: RealShell,
    fs: RealFs,
}

impl WorkflowIo for RealIo {
    type Timer = RealTimer;
    type Shell = RealShell;
    type Fs = RealFs;

    fn timer(&self) -> &Self::Timer {
        &self.timer
    }
    fn shell(&self) -> &Self::Shell {
        &self.shell
    }
    fn fs(&self) -> &Self::Fs {
        &self.fs
    }
}

/// Production IO with project filesystem operations delegated to the worker.
#[derive(Clone)]
pub struct WorkerIo {
    timer: RealTimer,
    shell: WorkerShellFactory,
    fs: WorkerFs,
}

impl WorkerIo {
    pub fn new(client: WorkerClient) -> Self {
        Self {
            timer: RealTimer,
            shell: WorkerShellFactory {
                client: client.clone(),
            },
            fs: WorkerFs { client },
        }
    }
}

impl WorkflowIo for WorkerIo {
    type Timer = RealTimer;
    type Shell = WorkerShellFactory;
    type Fs = WorkerFs;

    fn timer(&self) -> &Self::Timer {
        &self.timer
    }

    fn shell(&self) -> &Self::Shell {
        &self.shell
    }

    fn fs(&self) -> &Self::Fs {
        &self.fs
    }
}

/// `tokio::time::sleep` + `tokio::spawn`.
#[derive(Clone, Copy, Default)]
pub struct RealTimer;

impl WorkflowTimer for RealTimer {
    fn sleep(
        &self,
        duration: Duration,
        cancel: Arc<Notify>,
        closed: Arc<WorkflowClosed>,
    ) -> Pin<Box<dyn Future<Output = SleepOutcome> + Send>> {
        Box::pin(async move {
            let cancel = cancel.notified();
            let sleep = tokio::time::sleep(duration);
            tokio::pin!(cancel);
            tokio::pin!(sleep);

            // Register the cancel waiter before the select so a pulse
            // racing us is held as a permit; `closed.closed()` does its
            // own register-before-check for the shutdown signal.
            cancel.as_mut().enable();

            tokio::select! {
                biased;
                () = &mut cancel => SleepOutcome::Cancelled,
                () = closed.closed() => SleepOutcome::Closed,
                () = &mut sleep => SleepOutcome::Fired,
            }
        })
    }
}

/// `frances_shell::Shell::spawn` passthrough.
#[derive(Clone, Copy, Default)]
pub struct RealShell;

impl WorkflowShell for RealShell {
    type Handle = Shell;

    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send {
        Shell::spawn(opts)
    }
}

#[derive(Clone)]
pub struct WorkerShellFactory {
    client: WorkerClient,
}

impl WorkflowShell for WorkerShellFactory {
    type Handle = WorkerShell;

    async fn spawn(&self, opts: ShellOptions) -> Result<WorkerShell, ShellError> {
        self.client
            .open_shell(WorkerShellOptions {
                cwd: opts.cwd,
                env: opts
                    .env
                    .into_iter()
                    .map(|(name, value)| {
                        (
                            name.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
                init_script: opts.init_script,
            })
            .await
            .map_err(worker_shell_error)
    }
}

impl WorkflowShellHandle for WorkerShell {
    fn set_output_sink(&mut self, sink: Option<tokio::sync::mpsc::UnboundedSender<ReadEvent>>) {
        WorkerShell::set_output_sink(self, sink);
    }

    async fn run_with_opts(
        &mut self,
        command: &str,
        options: RunOpts,
        wait: WaitOpts,
    ) -> Result<RunOutcome, ShellError> {
        WorkerShell::run_with_opts(self, command, options, wait)
            .await
            .map_err(worker_shell_error)
    }

    async fn keep_waiting(&mut self, wait: WaitOpts) -> Result<RunOutcome, ShellError> {
        WorkerShell::keep_waiting(self, wait)
            .await
            .map_err(worker_shell_error)
    }

    async fn kill_running(&mut self) -> Result<(), ShellError> {
        WorkerShell::kill_running(self)
            .await
            .map_err(worker_shell_error)
    }

    async fn set_var(&mut self, name: String, value: Vec<u8>) -> Result<(), ShellError> {
        WorkerShell::set_var(self, name, Content::from_bytes(value))
            .await
            .map_err(worker_shell_error)
    }

    async fn get_var(&mut self, name: String) -> Result<String, ShellError> {
        let content = WorkerShell::get_var(self, name)
            .await
            .map_err(worker_shell_error)?;
        let mut reader = content.into_async_read().await.map_err(ShellError::Io)?;
        let mut value = String::new();
        reader
            .read_to_string(&mut value)
            .await
            .map_err(ShellError::Io)?;
        Ok(value)
    }
}

fn worker_shell_error(error: frances_worker::ClientError) -> ShellError {
    ShellError::Io(io::Error::other(error))
}

impl WorkflowShellHandle for Shell {
    fn set_output_sink(&mut self, sink: Option<tokio::sync::mpsc::UnboundedSender<ReadEvent>>) {
        Shell::set_output_sink(self, sink);
    }

    async fn run_with_opts(
        &mut self,
        command: &str,
        options: RunOpts,
        wait: WaitOpts,
    ) -> Result<RunOutcome, ShellError> {
        Shell::run_with_opts(self, command, options, wait).await
    }

    async fn keep_waiting(&mut self, wait: WaitOpts) -> Result<RunOutcome, ShellError> {
        Shell::keep_waiting(self, wait).await
    }

    async fn kill_running(&mut self) -> Result<(), ShellError> {
        Shell::kill_running(self).await
    }

    async fn set_var(&mut self, name: String, value: Vec<u8>) -> Result<(), ShellError> {
        let outcome = self
            .run_with_opts(
                &format!("export {name}=$(cat)"),
                RunOpts {
                    stdin: Some(value),
                    persist: vec![name],
                },
                WaitOpts::default(),
            )
            .await?;
        require_done(outcome, "set variable").map(|_| ())
    }

    async fn get_var(&mut self, name: String) -> Result<String, ShellError> {
        let outcome = self
            .run(
                &format!("( set -u; printf '%s' \"${name}\" )"),
                WaitOpts::default(),
            )
            .await?;
        require_done(outcome, "get variable")
    }
}

fn require_done(outcome: RunOutcome, operation: &str) -> Result<String, ShellError> {
    match outcome {
        RunOutcome::Done {
            exit_code: 0,
            output,
        } => Ok(output),
        other => Err(ShellError::Io(io::Error::other(format!(
            "{operation}: {other:?}"
        )))),
    }
}

/// `tokio::fs` passthrough.
#[derive(Clone, Copy, Default)]
pub struct RealFs;

#[derive(Clone)]
pub struct WorkerFs {
    client: WorkerClient,
}

impl WorkflowFs for WorkerFs {
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let content = self.client.read(path).await.map_err(worker_io_error)?;
        let mut reader = content.into_async_read().await?;
        let mut text = String::new();
        reader.read_to_string(&mut text).await?;
        Ok(text)
    }

    async fn write(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        self.client
            .write(path, Content::from_bytes(content.to_vec()))
            .await
            .map_err(worker_io_error)
    }

    async fn write_create_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        self.client
            .write_create_new(path, Content::from_bytes(content.to_vec()))
            .await
            .map_err(worker_io_error)
    }

    async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        let metadata = self.client.metadata(path).await.map_err(worker_io_error)?;
        Ok(FsMetadata {
            mtime_ns: metadata.mtime_ns,
            size: metadata.size,
            is_dir: metadata.is_dir,
        })
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.client
            .create_dir_all(path)
            .await
            .map_err(worker_io_error)
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.client
            .canonicalize(path)
            .await
            .map_err(worker_io_error)
    }

    async fn find_or_grep(&self, options: FileSearchOptions) -> io::Result<FileSearchResults> {
        let mut feed = self
            .client
            .find_or_grep(options)
            .await
            .map_err(worker_io_error)?;
        let mut collector = FileSearchCollector::default();
        loop {
            match feed
                .next()
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::UnexpectedEof, error))?
            {
                Some(FileSearchEvent::Done { truncated_at }) => {
                    return Ok(collector.finish(truncated_at));
                }
                Some(FileSearchEvent::Error { error }) => {
                    return Err(io::Error::other(error.error.message));
                }
                Some(event) => collector.push(event)?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "worker search feed ended without completion",
                    ));
                }
            }
        }
    }
}

fn worker_io_error(error: frances_worker::ClientError) -> io::Error {
    let kind = match &error {
        frances_worker::ClientError::Worker {
            code: ErrorCode::AlreadyExists,
            ..
        } => io::ErrorKind::AlreadyExists,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

impl WorkflowFs for RealFs {
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        tokio::fs::read_to_string(path).await
    }

    async fn write(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        tokio::fs::write(path, content).await
    }

    async fn write_create_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, content).await
    }

    async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        let meta = tokio::fs::metadata(path).await?;
        let modified = meta.modified()?;
        let dur = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| io::Error::other(format!("mtime before epoch: {e}")))?;
        let mtime_ns = i64::try_from(dur.as_nanos())
            .map_err(|e| io::Error::other(format!("mtime overflow: {e}")))?;
        Ok(FsMetadata {
            mtime_ns,
            size: meta.len(),
            is_dir: meta.is_dir(),
        })
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        tokio::fs::canonicalize(path).await
    }

    async fn find_or_grep(&self, options: FileSearchOptions) -> io::Result<FileSearchResults> {
        tokio::task::spawn_blocking(move || {
            let collector = Arc::new(parking_lot::Mutex::new(FileSearchCollector::default()));
            let emitted = collector.clone();
            let outcome = frances_worker::find_or_grep(
                options,
                || false,
                move |event| {
                    emitted
                        .lock()
                        .push(event)
                        .expect("local search emits consistent events");
                    true
                },
            )
            .map_err(io::Error::other)?;
            let frances_worker::SearchOutcome::Done { truncated_at } = outcome else {
                return Err(io::Error::other("local search cancelled"));
            };
            let collector = Arc::try_unwrap(collector)
                .map_err(|_| io::Error::other("local search collector is still shared"))?
                .into_inner();
            Ok(collector.finish(truncated_at))
        })
        .await
        .map_err(|error| io::Error::other(format!("search task: {error}")))?
    }
}
