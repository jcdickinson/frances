//! Production IO impls. `RealIo` is what the session runtime hands to
//! `WorkflowDepsImpl`; tests drag in [`super::mock::MockIo`] instead.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Notify;

use frances_shell::{Shell, ShellError, ShellOptions};

use super::{FsMetadata, SleepOutcome, WorkflowFs, WorkflowIo, WorkflowShell, WorkflowTimer};
use crate::closed::WorkflowClosed;

/// Production IO bundle. Unit-struct sub-impls, so cloning is free.
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

/// `tokio::time::sleep` + `tokio::spawn`. The current production
/// behaviour — lifted out of `modules/io.rs` so the test path can
/// swap it.
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
    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send {
        Shell::spawn(opts)
    }
}

/// `tokio::fs` passthrough. `WorkflowFs` is async on purpose: the JS
/// thread is a `current_thread` runtime, and the old `std::fs` calls
/// in `modules/file.rs` blocked it on every read/write.
#[derive(Clone, Copy, Default)]
pub struct RealFs;

impl WorkflowFs for RealFs {
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        tokio::fs::read_to_string(path).await
    }

    async fn write(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        tokio::fs::write(path, content).await
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
        })
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }
}
