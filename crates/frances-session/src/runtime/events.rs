//! In-process events channel.
//!
//! The session runtime publishes [`StreamFrame`]s into an mpsc; the TUI
//! drains them through the matching receiver. The channel exists for
//! the lifetime of the [`crate::runtime::SessionRuntime`] — there is no
//! reattach race, no socket pairing, no per-client buffering.
//!
//! Writers ignore send failures: a closed receiver means the TUI has
//! quit and the runtime is on its way to shutdown. Persistent state
//! (scrollback rows, history rows) is written to the DB inside the
//! workflow path, so a dropped frame does not lose anything that's
//! supposed to survive a restart.

use tokio::sync::mpsc;

use crate::events::StreamFrame;

#[derive(Clone)]
pub struct EventsChannel {
    tx: mpsc::UnboundedSender<StreamFrame>,
}

impl EventsChannel {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<StreamFrame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    pub fn send(&self, frame: StreamFrame) {
        let _ = self.tx.send(frame);
    }
}
