use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::provider::ProviderError;

/// Errors raised by built-in providers when reading or parsing a source.
#[derive(Debug, Error)]
pub enum SourceLoadError<E: std::error::Error + Send + Sync + 'static> {
    #[error("could not read config source `{name}`: {source}")]
    Read {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse config source `{name}`: {source}")]
    Parse {
        name: String,
        #[source]
        source: E,
    },
}

/// Errors raised when binding a section of the [`Configuration`] to a typed
/// struct.
///
/// [`Configuration`]: crate::Configuration
#[derive(Debug, Clone, Error)]
pub enum ConfigBindError {
    #[error("{path}: cannot convert `{value}` to {target}")]
    TypeConversion {
        path: Arc<str>,
        target: &'static str,
        value: Arc<str>,
    },
    #[error("{path}: required value missing for {target_type}")]
    RequiredValue {
        path: Arc<str>,
        target_type: &'static str,
    },
    #[error("{path}: structural error binding {target_type}: {error}")]
    Structural {
        path: Arc<str>,
        target_type: &'static str,
        error: Arc<str>,
    },
    #[error("{path}: required section not provided")]
    RequiredSection { path: Arc<str> },
}

impl ConfigBindError {
    pub(crate) fn path(&self) -> &Arc<str> {
        match self {
            ConfigBindError::TypeConversion { path, .. }
            | ConfigBindError::RequiredValue { path, .. }
            | ConfigBindError::Structural { path, .. }
            | ConfigBindError::RequiredSection { path } => path,
        }
    }

    /// Prepend `prefix` to this error's path. Used by the deserializer when
    /// surfacing errors from nested fields.
    pub(crate) fn add_path(self, prefix: &str) -> Self {
        if prefix.is_empty() {
            return self;
        }
        let combined: Arc<str> = if self.path().is_empty() {
            Arc::from(prefix)
        } else {
            Arc::from(format!(
                "{prefix}{}{}",
                crate::value::SEPARATOR,
                self.path()
            ))
        };
        match self {
            ConfigBindError::TypeConversion { target, value, .. } => {
                ConfigBindError::TypeConversion {
                    path: combined,
                    target,
                    value,
                }
            }
            ConfigBindError::RequiredValue { target_type, .. } => ConfigBindError::RequiredValue {
                path: combined,
                target_type,
            },
            ConfigBindError::Structural {
                target_type, error, ..
            } => ConfigBindError::Structural {
                path: combined,
                target_type,
                error,
            },
            ConfigBindError::RequiredSection { .. } => {
                ConfigBindError::RequiredSection { path: combined }
            }
        }
    }
}

impl serde::de::Error for ConfigBindError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        ConfigBindError::Structural {
            path: Arc::from(""),
            target_type: "<unknown>",
            error: Arc::from(msg.to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("provider failed during initial load: {0}")]
    Provider(#[from] ProviderError),
    #[error("event processor terminated before initial load completed")]
    ProcessorGone,
}

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("event processor terminated")]
    ProcessorGone,
}
