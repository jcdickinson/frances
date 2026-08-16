//! IO seam for the workflow runtime.
//!
//! Three peer sub-traits — [`WorkflowTimer`], [`WorkflowShell`],
//! [`WorkflowFs`] — sit under an umbrella [`WorkflowIo`] trait.
//!
//! Production wires up [`real::RealIo`]; tests drag in
//! [`mock::MockIo`] from the `test-utils` feature.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, mpsc::UnboundedSender};

use frances_shell::{ReadEvent, RunOpts, RunOutcome, ShellError, ShellOptions, WaitOpts};

use crate::closed::WorkflowClosed;

pub mod real;

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;

/// Why a sleep settled. The string forms (`"fired"` / `"closed"` /
/// `"cancelled"`) are what `SleepToken` exposes to JS; this enum is the
/// Rust-side equivalent that timer impls return to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepOutcome {
    /// The requested duration elapsed naturally.
    Fired,
    /// The workflow began tearing down before the duration elapsed.
    Closed,
    /// `_clearSleep` was called, or the token was dropped.
    Cancelled,
}

impl SleepOutcome {
    /// Wire string used by `SleepToken::then(onF, onR)` in JS.
    pub fn as_wire(self) -> &'static str {
        match self {
            SleepOutcome::Fired => "fired",
            SleepOutcome::Closed => "closed",
            SleepOutcome::Cancelled => "cancelled",
        }
    }
}

/// File metadata the host reads: mtime and size.
#[derive(Debug, Clone, Copy)]
pub struct FsMetadata {
    /// Last-modified time as nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// File size in bytes.
    pub size: u64,
    /// Whether the path names a directory.
    pub is_dir: bool,
}

/// The umbrella IO surface — held by `WorkflowDeps::Io`.
pub trait WorkflowIo: Clone + Send + Sync + 'static {
    type Timer: WorkflowTimer;
    type Shell: WorkflowShell;
    type Fs: WorkflowFs;

    fn timer(&self) -> &Self::Timer;
    fn shell(&self) -> &Self::Shell;
    fn fs(&self) -> &Self::Fs;
}

/// The clock backing `_setSleep` (and indirectly the JS `Timer` class).
///
/// Implementations must observe all three signals:
///
/// - `duration` elapsed → [`SleepOutcome::Fired`]
/// - `cancel` notify → [`SleepOutcome::Cancelled`]
/// - `closed` → [`SleepOutcome::Closed`]
pub trait WorkflowTimer: Clone + Send + Sync + 'static {
    fn sleep(
        &self,
        duration: Duration,
        cancel: Arc<Notify>,
        closed: Arc<WorkflowClosed>,
    ) -> Pin<Box<dyn Future<Output = SleepOutcome> + Send>>;
}

/// Spawns shell handles for the `frances:v1/tools/shell` primitive.
pub trait WorkflowShell: Clone + Send + Sync + 'static {
    type Handle: WorkflowShellHandle;

    fn spawn(
        &self,
        opts: ShellOptions,
    ) -> impl Future<Output = Result<Self::Handle, ShellError>> + Send;
}

pub trait WorkflowShellHandle: Send + 'static {
    fn set_output_sink(&mut self, sink: Option<UnboundedSender<ReadEvent>>);

    fn run_with_opts(
        &mut self,
        command: &str,
        options: RunOpts,
        wait: WaitOpts,
    ) -> impl Future<Output = Result<RunOutcome, ShellError>> + Send;

    fn keep_waiting(
        &mut self,
        wait: WaitOpts,
    ) -> impl Future<Output = Result<RunOutcome, ShellError>> + Send;

    fn kill_running(&mut self) -> impl Future<Output = Result<(), ShellError>> + Send;

    fn set_var(
        &mut self,
        name: String,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<(), ShellError>> + Send;

    fn get_var(&mut self, name: String) -> impl Future<Output = Result<String, ShellError>> + Send;
}

/// Filesystem accessor backing `frances:v1/tools/file` reads/writes and
/// `file_find_or_grep`'s binary-detection peek.
pub trait WorkflowFs: Clone + Send + Sync + 'static {
    fn read_to_string(&self, path: &Path) -> impl Future<Output = std::io::Result<String>> + Send;

    fn write(
        &self,
        path: &Path,
        content: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> + Send;

    fn write_create_new(
        &self,
        path: &Path,
        content: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> + Send;

    fn metadata(&self, path: &Path) -> impl Future<Output = std::io::Result<FsMetadata>> + Send;

    fn create_dir_all(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> + Send;

    /// Resolve symlinks and return the canonical path. For in-memory
    /// filesystems that have no symlinks, returning `path` as-is is
    /// correct.
    fn canonicalize(&self, path: &Path) -> impl Future<Output = std::io::Result<PathBuf>> + Send;
}
