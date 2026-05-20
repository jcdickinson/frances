//! Per-attach events socket.
//!
//! There's exactly one daemon-to-client frame stream per session, opened
//! by the TUI once at attach time and held for the life of the
//! connection. The daemon writes:
//!
//! 1. The initial scrollback replay burst from `attach`.
//! 2. All workflow output frames driven by `prompt` calls.
//! 3. Mid-cycle replays when the active workflow changes
//!    (`ScrollbackReset { instance_id }` + replay + `ScrollbackReplayEnd`).
//!
//! all through the same stream. `StreamFrame::Done` is a per-prompt
//! boundary marker, not a stream terminator.
//!
//! ## Synchronisation
//!
//! The TUI opens the events socket before calling `attach` RPC, so by
//! the time the attach handler runs the stream is usually already in
//! place. To survive the race where the handler arrives first, the
//! handler awaits [`EventsSocket::wait_for_stream`] with a short
//! timeout before writing the initial replay.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tracing::{trace, warn};

use super::ServerState;

const EVENTS_PAIRING_TIMEOUT: Duration = Duration::from_secs(5);

/// Owns the single per-session events stream. Writers
/// (`attach` / `prompt` handlers) acquire the lock for the duration of
/// their write or workflow cycle, serialising all writes through one
/// `UnixStream`.
#[derive(Default)]
pub(crate) struct EventsSocket {
    inner: AsyncMutex<Option<UnixStream>>,
    /// Notified each time the socket transitions from `None` to
    /// `Some(_)`. Callers awaiting an attach-time stream wait on this.
    arrival: Notify,
}

impl EventsSocket {
    /// Install a freshly-accepted events stream. Replaces any
    /// previously-installed stream (the previous TUI disconnected; the
    /// daemon's `client_attached` gate keeps this from happening
    /// concurrently in practice). Notifies waiters.
    pub async fn install(&self, stream: UnixStream) {
        let mut guard = self.inner.lock().await;
        *guard = Some(stream);
        self.arrival.notify_waiters();
    }

    /// Wait (up to [`EVENTS_PAIRING_TIMEOUT`]) for the events socket
    /// to be installed. Returns `true` if a stream is present by the
    /// deadline, `false` otherwise.
    pub async fn wait_for_stream(&self) -> bool {
        let mut deadline = std::pin::pin!(tokio::time::sleep(EVENTS_PAIRING_TIMEOUT));
        loop {
            {
                let guard = self.inner.lock().await;
                if guard.is_some() {
                    return true;
                }
            }
            tokio::select! {
                () = self.arrival.notified() => {},
                () = &mut deadline => return false,
            }
        }
    }

    /// Acquire the events stream for exclusive read-write access. The
    /// guard's `&mut UnixStream` is the canonical sink for all daemon
    /// → TUI frames. While the guard is held, no other writer can
    /// access the stream. Returns `None` if no stream is installed.
    pub async fn lock(&self) -> EventsGuard<'_> {
        EventsGuard {
            inner: self.inner.lock().await,
        }
    }
}

pub(crate) struct EventsGuard<'a> {
    inner: tokio::sync::MutexGuard<'a, Option<UnixStream>>,
}

impl EventsGuard<'_> {
    pub fn stream(&mut self) -> Option<&mut UnixStream> {
        self.inner.as_mut()
    }

    /// Drop the installed stream (e.g. after a write failure or on
    /// detach). The next attach reinstalls a fresh one.
    pub fn drop_stream(&mut self) {
        *self.inner = None;
    }
}

pub(super) async fn accept_events(listener: UnixListener, state: Arc<ServerState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                trace!("events socket installed");
                state.events.install(stream).await;
            }
            Err(error) => {
                warn!(%error, "events accept error");
                return;
            }
        }
    }
}
