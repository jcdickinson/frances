//! Hierarchical, layered configuration for frances.
//!
//! Sources (env vars, TOML files) push [`ConfigEvent`]s
//! into a [`ConfigHandle`]. The handle keeps an immutable [`Configuration`]
//! snapshot, replaces it on each event, and refreshes typed [`ConfigBinding`]s
//! that subscribers can poll via [`ConfigBinding::get`] or observe via
//! [`ConfigBinding::subscribe`].
//!
//! Path syntax uses `::` between segments, e.g. `llm::model` or
//! `tags::0::name`. Numeric segments parse as [`Value::Int`] so array indices
//! from TOML and stringly-typed indices from env vars collide on the same
//! tree key.

mod binding;
mod config;
mod deserializer;
mod env_string;
mod error;
mod event;
mod handle;
mod provider;
mod providers;
pub mod util;
mod value;

pub use binding::{ConfigBinding, ConfigBindingRef, RequiredConfigBinding};
pub use config::{Configuration, ConfigurationRef};
pub use env_string::{EnvLookup, EnvString, EnvStringExpandError};
pub use error::{BuildError, ConfigBindError, MapError, SourceLoadError};
pub use event::{ConfigEvent, EventSender, SendError};
pub use handle::{ConfigHandle, ConfigHandleRef};
pub use provider::{ConfigProvider, ProviderError};
#[cfg(feature = "test-util")]
pub use providers::InMemoryProvider;
pub use providers::{EnvProvider, TomlProvider};
pub use util::{Keys, KeysDiff};
pub use value::{Path, SEPARATOR, Value};
