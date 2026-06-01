use async_trait::async_trait;

use crate::event::{ConfigEvent, EventSender};
use crate::provider::{ConfigProvider, ProviderError};
use crate::value::{Path, Value};

/// In-memory `ConfigProvider` built from a pre-baked list of
/// `(path, value)` pairs. Emits everything in a single batch on `load`.
///
/// Test-only: exposed under the `test-util` feature so downstream
/// crates can spin up a `ConfigHandle` without writing TOML to disk.
#[derive(Debug, Clone, Default)]
pub struct InMemoryProvider {
    events: Vec<ConfigEvent>,
}

impl InMemoryProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a single `(path, value)` pair to the initial event batch.
    pub fn set(mut self, path: impl Into<Path>, value: impl Into<Value>) -> Self {
        self.events.push(ConfigEvent::new(path, value));
        self
    }
}

#[async_trait]
impl ConfigProvider for InMemoryProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        if self.events.is_empty() {
            return Ok(());
        }
        events
            .send(self.events.clone())
            .await
            .map_err(ProviderError::new)?;
        Ok(())
    }
}
