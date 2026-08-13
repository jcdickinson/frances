//! In-process events channel.
//!
//! The session runtime publishes [`StreamFrame`]s into an mpsc; the UI
//! drains them through the matching receiver. The channel exists for
//! the lifetime of the [`crate::runtime::SessionRuntime`] — there is no
//! reattach race, no socket pairing, no per-client buffering.
//!
//! Writers ignore send failures silently.

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
