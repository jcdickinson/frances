use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::value::{Path, Value};

/// A single config-source observation.
///
/// Providers emit one of these per `(path, value)` pair they discover, both
/// during initial load and (optionally) at runtime. Use [`Value::Null`] to
/// signal that a previously-set key has been unset.
#[derive(Debug, Clone)]
pub struct ConfigEvent {
    pub path: Path,
    pub value: Value,
}

impl ConfigEvent {
    pub fn new(path: impl Into<Path>, value: impl Into<Value>) -> Self {
        Self {
            path: path.into(),
            value: value.into(),
        }
    }

    pub fn unset(path: impl Into<Path>) -> Self {
        Self {
            path: path.into(),
            value: Value::Null,
        }
    }
}

/// The sender handed to providers' `load()`. Wraps the handle's internal
/// channel so events go directly into the processor's queue — no
/// intermediate forwarder, no barrier race.
#[derive(Clone)]
pub struct EventSender {
    pub(crate) inner: mpsc::Sender<InternalEvent>,
}

impl EventSender {
    /// Send a batch of events. The batch is applied atomically — the
    /// processor walks every event into the snapshot before fanning out a
    /// single binding-refresh pass. Providers emitting one event wrap it
    /// in a one-element `Vec`.
    pub async fn send(&self, events: Vec<ConfigEvent>) -> Result<(), SendError> {
        self.inner
            .send(InternalEvent::Batch(events))
            .await
            .map_err(|_| SendError)
    }
}

/// Error returned by [`EventSender::send`] when the receiver has been
/// dropped (e.g. the [`ConfigHandle`] was dropped).
///
/// [`ConfigHandle`]: crate::ConfigHandle
#[derive(Debug, Error)]
#[error("config event channel closed")]
pub struct SendError;

/// Internal event wrapper. Public events flow through `Batch`; `Barrier`
/// is a oneshot sent after all providers have called `load` to ensure the
/// processor has applied the initial event burst before [`build`] returns.
///
/// [`build`]: crate::ConfigHandle::build
pub(crate) enum InternalEvent {
    Batch(Vec<ConfigEvent>),
    Barrier(oneshot::Sender<()>),
}
