//! Test IO bundle. `MockIo` exposes a virtual clock, an in-memory
//! filesystem, and (by default) a no-bash shell. Each sub-piece is
//! independently swappable so a test can drag in the real shell while
//! keeping mock timer + mock fs.
//!
//! Clock semantics — picked deliberately to be the most deterministic
//! variant: `MockTimer::advance(Duration)` walks the pending-sleep
//! heap synchronously and settles every entry whose deadline ≤ new
//! `now`. `advance` only returns once the settled waiters have been
//! notified.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;

use frances_shell::{Shell, ShellError, ShellOptions};

use super::real::{RealFs, RealTimer};
use super::{FsMetadata, SleepOutcome, WorkflowFs, WorkflowIo, WorkflowShell, WorkflowTimer};
use crate::closed::WorkflowClosed;

/// Test IO bundle. Generic over the three sub-pieces so any one of
/// them can be dragged in independently — picking a mock timer
/// without losing real fs, real shell without losing mock timer, etc.
///
/// Defaults match today's workflow-test semantics: real timer
/// (existing tests measure real elapsed time), `MockShell` (errors on
/// spawn unless `with_real_shell` is used), and real fs (tests seed a
/// tempdir + `set_cwd`). The driver suite and any new clock-sensitive
/// test picks `StubIo<MockTimer, MockShell, MockFs>` instead.
#[derive(Clone, Default)]
pub struct StubIo<T = RealTimer, S = MockShell, F = RealFs> {
    timer: T,
    shell: S,
    fs: F,
}

/// Convenience alias for tests that want full determinism — virtual
/// clock + scripted shell + in-memory fs.
pub type MockIo = StubIo<MockTimer, MockShell, MockFs>;

impl<T, S, F> StubIo<T, S, F> {
    pub fn timer(&self) -> &T {
        &self.timer
    }
    pub fn shell(&self) -> &S {
        &self.shell
    }
    pub fn fs(&self) -> &F {
        &self.fs
    }
}

impl<S, F> StubIo<RealTimer, S, F>
where
    S: Default,
    F: Default,
{
    /// Construct with `RealTimer` + the defaults for shell + fs.
    /// Existing workflow tests rely on real time elapsing; this is
    /// what `StubDeps::default()` reaches for.
    pub fn with_real_timer() -> Self {
        Self::default()
    }
}

impl<T, F> StubIo<T, MockShell, F>
where
    T: Default,
    F: Default,
{
    /// Construct with a real-bash `MockShell` plus default timer/fs.
    /// Used by `StubDepsRealShell` for tests that need an actual
    /// subprocess.
    pub fn with_real_shell() -> Self {
        Self {
            timer: T::default(),
            shell: MockShell::with_real(),
            fs: F::default(),
        }
    }
}

impl<T, S, F> WorkflowIo for StubIo<T, S, F>
where
    T: WorkflowTimer,
    S: WorkflowShell,
    F: WorkflowFs,
{
    type Timer = T;
    type Shell = S;
    type Fs = F;

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

// ---------------- timer ----------------

/// Virtual clock. `now_ns` is the monotonic tick; `pending` holds the
/// deadlines of unfired sleeps, keyed by a per-sleep id so we can
/// settle them in deadline order via a `BTreeMap<(deadline_ns, id)>`.
#[derive(Clone, Default)]
pub struct MockTimer {
    inner: Arc<MockTimerInner>,
}

#[derive(Default)]
struct MockTimerInner {
    now_ns: AtomicU64,
    next_id: AtomicU64,
    /// Ordered by `(deadline_ns, id)` so siblings at the same deadline
    /// settle in registration order.
    pending: Mutex<BTreeMap<(u64, u64), Arc<SleepSlot>>>,
}

/// One pending sleep. `settle` flips when the timer fires it; the
/// future polls `notify` to learn that.
struct SleepSlot {
    settled: Mutex<Option<SleepOutcome>>,
    notify: Notify,
}

impl MockTimer {
    /// Advance virtual time by `delta`. Every pending sleep whose
    /// deadline is ≤ the new `now` is settled with `Fired`
    /// synchronously inside this call. Returns once all settled
    /// waiters have been notified (the `Notify` pulses immediately —
    /// the test still has to drive the JS runtime for the JS-side
    /// promise to resolve).
    pub fn advance(&self, delta: Duration) {
        let inner = &self.inner;
        let new_now = inner
            .now_ns
            .fetch_add(delta.as_nanos() as u64, Ordering::AcqRel)
            + delta.as_nanos() as u64;

        // Collect the slots that need firing without holding the lock
        // across the `notify_waiters` calls.
        let mut to_fire = Vec::new();
        {
            let mut pending = inner.pending.lock();
            let still_pending = pending.split_off(&(new_now + 1, 0));
            // `split_off` leaves entries < new_now+1 in `pending`,
            // moves the rest into `still_pending`. We want the
            // opposite: keep what's still future, fire what's due.
            for (k, slot) in pending.iter() {
                to_fire.push((*k, slot.clone()));
            }
            *pending = still_pending;
        }
        for (_, slot) in to_fire {
            let mut guard = slot.settled.lock();
            if guard.is_none() {
                *guard = Some(SleepOutcome::Fired);
                slot.notify.notify_waiters();
            }
        }
    }

    /// Current virtual time in nanoseconds since the test started.
    pub fn now_ns(&self) -> u64 {
        self.inner.now_ns.load(Ordering::Acquire)
    }
}

impl WorkflowTimer for MockTimer {
    fn sleep(
        &self,
        duration: Duration,
        cancel: Arc<Notify>,
        closed: Arc<WorkflowClosed>,
    ) -> Pin<Box<dyn Future<Output = SleepOutcome> + Send>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            // Fast-path: workflow already closed.
            if closed.is_closed() {
                return SleepOutcome::Closed;
            }

            // Compute deadline against the virtual clock at the moment
            // the sleep was constructed.
            let now = inner.now_ns.load(Ordering::Acquire);
            let deadline = now.saturating_add(duration.as_nanos() as u64);
            let id = inner.next_id.fetch_add(1, Ordering::AcqRel);
            let slot = Arc::new(SleepSlot {
                settled: Mutex::new(None),
                notify: Notify::new(),
            });

            // Register before reading so any advance() that fires
            // between the construction and the wait is held as a
            // permit on the Notified future.
            {
                let mut pending = inner.pending.lock();
                // If the clock has already moved past the deadline
                // (test advanced before sleep was registered), fire
                // immediately.
                let cur = inner.now_ns.load(Ordering::Acquire);
                if deadline <= cur {
                    *slot.settled.lock() = Some(SleepOutcome::Fired);
                } else {
                    pending.insert((deadline, id), slot.clone());
                }
            }

            let cancel_n = cancel.notified();
            let slot_n = slot.notify.notified();
            tokio::pin!(cancel_n);
            tokio::pin!(slot_n);
            cancel_n.as_mut().enable();
            slot_n.as_mut().enable();

            // If `advance` already settled us, skip the select.
            if let Some(outcome) = *slot.settled.lock() {
                return outcome;
            }
            if closed.is_closed() {
                return SleepOutcome::Closed;
            }

            // `closed.closed()` does its own register-before-check, so a
            // close racing the line above is still observed here.
            let outcome = tokio::select! {
                biased;
                () = &mut cancel_n => SleepOutcome::Cancelled,
                () = closed.closed() => SleepOutcome::Closed,
                () = &mut slot_n => {
                    slot.settled.lock().unwrap_or(SleepOutcome::Fired)
                }
            };

            // De-register so `advance` doesn't try to settle a slot
            // whose future has already moved on.
            {
                let mut pending = inner.pending.lock();
                pending.remove(&(deadline, id));
            }
            outcome
        })
    }
}

// ---------------- shell ----------------

/// Test shell. Default impl errors on every spawn — matches today's
/// `StubShellFactory`. Tests that need real bash construct
/// `MockShell::with_real()`; tests that want to script bash output
/// will (later) build on this; out of scope for the first cut.
#[derive(Clone, Default)]
pub struct MockShell {
    real: bool,
}

impl MockShell {
    /// Toggle to the real `frances_shell::Shell::spawn` path. Equivalent
    /// to the old `StubDepsRealShell`/`RealShellFactory` pair.
    pub fn with_real() -> Self {
        Self { real: true }
    }
}

impl WorkflowShell for MockShell {
    fn spawn(&self, opts: ShellOptions) -> impl Future<Output = Result<Shell, ShellError>> + Send {
        let real = self.real;
        async move {
            if real {
                Shell::spawn(opts).await
            } else {
                Err(ShellError::Handshake(
                    "MockShell: real bash not enabled (use MockShell::with_real)".to_owned(),
                ))
            }
        }
    }
}

// ---------------- fs ----------------

/// In-memory filesystem. One mutex per call site — kept simple over
/// fast; tests don't hit this in hot loops.
#[derive(Clone, Default)]
pub struct MockFs {
    inner: Arc<Mutex<MockFsInner>>,
}

#[derive(Default)]
struct MockFsInner {
    files: HashMap<PathBuf, Vec<u8>>,
    /// `mtime_ns` per path. Auto-advances on every write so the
    /// edit-engine's mtime-keyed loop guard sees fresh stamps without
    /// the test having to call `touch` itself.
    mtime: HashMap<PathBuf, i64>,
    /// Per-`MockFs` clock used to assign mtimes. Each write bumps it
    /// by one — that's enough for the loop guard since it compares
    /// for equality, not real wall-clock ordering.
    mtime_counter: i64,
    dirs: std::collections::HashSet<PathBuf>,
}

impl MockFs {
    /// Seed `path` with `content`. Used by tests to pre-populate.
    pub fn write_file(&self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        let mut inner = self.inner.lock();
        let p = path.into();
        let next = inner.mtime_counter.wrapping_add(1);
        inner.mtime_counter = next;
        inner.files.insert(p.clone(), content.into());
        inner.mtime.insert(p, next);
    }

    /// Read the in-memory bytes for `path`. Returns `None` if no
    /// file was seeded or written there.
    pub fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        self.inner.lock().files.get(path).cloned()
    }
}

impl WorkflowFs for MockFs {
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes =
            self.inner.lock().files.get(path).cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, path.display().to_string())
            })?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    async fn write(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let mut inner = self.inner.lock();
        let next = inner.mtime_counter.wrapping_add(1);
        inner.mtime_counter = next;
        inner.files.insert(path.to_path_buf(), content.to_vec());
        inner.mtime.insert(path.to_path_buf(), next);
        Ok(())
    }

    async fn metadata(&self, path: &Path) -> io::Result<FsMetadata> {
        let inner = self.inner.lock();
        let bytes = inner
            .files
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))?;
        let mtime_ns = *inner.mtime.get(path).unwrap_or(&0);
        Ok(FsMetadata {
            mtime_ns,
            size: bytes.len() as u64,
        })
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.lock().dirs.insert(path.to_path_buf());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timer_advance_fires_due_sleeps_synchronously() {
        let timer = MockTimer::default();
        let cancel = Arc::new(Notify::new());
        let closed = Arc::new(WorkflowClosed::default());

        let fut_100 = timer.sleep(Duration::from_millis(100), cancel.clone(), closed.clone());
        let fut_250 = timer.sleep(Duration::from_millis(250), cancel.clone(), closed.clone());
        // Spawn waiters so they're registered before we advance.
        let h100 = tokio::spawn(fut_100);
        let h250 = tokio::spawn(fut_250);
        // Yield so the spawned tasks reach the await point.
        tokio::task::yield_now().await;

        timer.advance(Duration::from_millis(150));
        assert_eq!(h100.await.unwrap(), SleepOutcome::Fired);
        // 250ms one is still pending.
        assert!(!h250.is_finished());

        timer.advance(Duration::from_millis(100));
        assert_eq!(h250.await.unwrap(), SleepOutcome::Fired);
    }

    #[tokio::test]
    async fn timer_honours_cancel() {
        let timer = MockTimer::default();
        let cancel = Arc::new(Notify::new());
        let closed = Arc::new(WorkflowClosed::default());

        let fut = timer.sleep(Duration::from_secs(60), cancel.clone(), closed.clone());
        let h = tokio::spawn(fut);
        tokio::task::yield_now().await;

        cancel.notify_waiters();
        assert_eq!(h.await.unwrap(), SleepOutcome::Cancelled);
    }

    #[tokio::test]
    async fn timer_honours_closed_flag() {
        let timer = MockTimer::default();
        let cancel = Arc::new(Notify::new());
        let closed = Arc::new(WorkflowClosed::default());

        let fut = timer.sleep(Duration::from_secs(60), cancel.clone(), closed.clone());
        let h = tokio::spawn(fut);
        tokio::task::yield_now().await;

        closed.close();
        assert_eq!(h.await.unwrap(), SleepOutcome::Closed);
    }

    #[tokio::test]
    async fn fs_read_write_roundtrip() {
        let fs = MockFs::default();
        fs.write(Path::new("/tmp/a"), b"hello").await.unwrap();
        let s = fs.read_to_string(Path::new("/tmp/a")).await.unwrap();
        assert_eq!(s, "hello");
        let meta = fs.metadata(Path::new("/tmp/a")).await.unwrap();
        assert_eq!(meta.size, 5);
        // Two writes must produce different mtimes so the loop guard
        // can tell them apart.
        let first = meta.mtime_ns;
        fs.write(Path::new("/tmp/a"), b"hello!").await.unwrap();
        let second = fs.metadata(Path::new("/tmp/a")).await.unwrap().mtime_ns;
        assert_ne!(first, second);
    }
}
