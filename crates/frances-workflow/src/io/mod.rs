//! IO seam for the workflow runtime.
//!
//! Three peer sub-traits — [`WorkflowTimer`], [`WorkflowShell`],
//! [`WorkflowFs`] — sit under an umbrella [`WorkflowIo`] trait. The
//! umbrella exists so callers (production and tests) handle a single
//! `Io` value through `WorkflowDeps::io()`, but each sub-piece is its
//! own trait so a test can drag in the real shell while keeping a mock
//! timer + mock fs (or any other combination).
//!
//! Production wires up [`real::RealIo`]; tests drag in
//! [`mock::MockIo`] from the `test-utils` feature.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use frances_shell::{Shell, ShellError, ShellOptions};

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

/// File metadata the host reads. Narrower than `std::fs::Metadata` —
/// just the fields `modules/file.rs` actually consumes (mtime, size).
#[derive(Debug, Clone, Copy)]
pub struct FsMetadata {
    /// Last-modified time as nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// File size in bytes.
    pub size: u64,
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
///
/// The returned future is `Send` and `'static` so the JS-side primitive
/// can spawn it onto the global runtime without dragging lifetimes
/// through rquickjs.
pub trait WorkflowTimer: Clone + Send + Sync + 'static {
    fn sleep(
        &self,
        duration: Duration,
        cancel: Arc<Notify>,
        closed: Arc<WorkflowClosed>,
    ) -> Pin<Box<dyn Future<Output = SleepOutcome> + Send>>;
}

/// Spawns bash subprocesses for the `frances:v1/tools/shell` primitive.
/// The returned `Shell` is the concrete `frances_shell::Shell`; this
/// trait stubs the *spawn* boundary only — `Shell` itself stays a
/// concrete struct, so test impls that need to stub shell behaviour
/// either inject the real shell ([`real::RealShell`]) or fail spawn.
pub trait WorkflowShell: Clone + Send + Sync + 'static {
    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send;
}

/// Filesystem accessor backing `frances:v1/tools/file` reads/writes and
/// `file_find_or_grep`'s binary-detection peek.
///
/// Mirrors what `modules/file.rs` actually needs — no full `Metadata`,
/// no directory walker (file_find_or_grep enumerates against the real
/// filesystem; only its per-file reads route through here).
pub trait WorkflowFs: Clone + Send + Sync + 'static {
    fn read_to_string(&self, path: &Path) -> impl Future<Output = std::io::Result<String>> + Send;

    fn write(
        &self,
        path: &Path,
        content: &[u8],
    ) -> impl Future<Output = std::io::Result<()>> + Send;

    fn metadata(&self, path: &Path) -> impl Future<Output = std::io::Result<FsMetadata>> + Send;

    fn create_dir_all(&self, path: &Path) -> impl Future<Output = std::io::Result<()>> + Send;
}
