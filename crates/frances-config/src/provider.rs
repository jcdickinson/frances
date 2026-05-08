use async_trait::async_trait;
use thiserror::Error;

use crate::event::EventSender;

/// A configuration source.
///
/// Calling [`load`](ConfigProvider::load) performs the initial bulk read.
/// The provider sends a [`ConfigEvent`] for every key/value pair it
/// discovers, awaiting each send so the receiver observes events in order.
/// `load` returns once the initial state has been fully emitted.
///
/// After `load` returns, the provider may continue to hold `events` and
/// publish further events at runtime (e.g. on a file-watcher tick, or in
/// response to a manual reload). Providers that have no runtime semantics
/// simply drop the sender.
#[async_trait]
pub trait ConfigProvider: Send + Sync + 'static {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError>;
}

/// A boxed error wrapper so different providers can fail with different
/// concrete error types without parameterising the trait.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ProviderError(pub Box<dyn std::error::Error + Send + Sync + 'static>);

impl ProviderError {
    pub fn new<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self(Box::new(err))
    }
}

impl<E> From<SourceLoadError<E>> for ProviderError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(err: SourceLoadError<E>) -> Self {
        Self::new(err)
    }
}

use crate::error::SourceLoadError;
