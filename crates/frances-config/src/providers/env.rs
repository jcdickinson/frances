use std::collections::HashMap;

use async_trait::async_trait;

use crate::event::{ConfigEvent, EventSender};
use crate::provider::{ConfigProvider, ProviderError};
use crate::value::{Path, Value};

/// Reads config from environment variables.
///
/// Translation rules:
/// - Names are lowercased.
/// - `__` (double underscore) splits a name into path segments.
/// - Numeric segments become [`Value::Int`]; everything else becomes
///   [`Value::String`].
/// - With a prefix `MYAPP`: only names starting with `MYAPP` (or `MYAPP_`)
///   are read; the prefix is stripped before splitting.
///
/// `EnvProvider` performs initial bulk load and does not retain `events`.
pub struct EnvProvider {
    prefix: Option<String>,
    source: EnvSource,
}

enum EnvSource {
    Process,
    Explicit(HashMap<String, String>),
}

impl EnvProvider {
    pub fn new() -> Self {
        Self {
            prefix: None,
            source: EnvSource::Process,
        }
    }

    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: Some(prefix.into()),
            source: EnvSource::Process,
        }
    }

    /// Construct a provider that reads from an explicit map instead of the
    /// process environment. Useful for tests and for callers that already
    /// pass env around as a slice/map.
    pub fn from_vars(prefix: Option<String>, vars: HashMap<String, String>) -> Self {
        Self {
            prefix,
            source: EnvSource::Explicit(vars),
        }
    }

    fn matching_pairs(&self) -> Vec<(String, String)> {
        let pairs: Box<dyn Iterator<Item = (String, String)>> = match &self.source {
            EnvSource::Process => Box::new(std::env::vars()),
            EnvSource::Explicit(map) => Box::new(map.iter().map(|(k, v)| (k.clone(), v.clone()))),
        };
        match &self.prefix {
            None => pairs.collect(),
            Some(prefix) => pairs
                .filter_map(|(k, v)| {
                    let stripped = strip_prefix_ci(&k, prefix)?;
                    Some((stripped.to_string(), v))
                })
                .collect(),
        }
    }
}

impl Default for EnvProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConfigProvider for EnvProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        let mut batch = Vec::new();
        for (key, value) in self.matching_pairs() {
            let path = key_to_path(&key);
            if path.is_empty() {
                continue;
            }
            batch.push(ConfigEvent::new(path, Value::String(value.into())));
        }
        if !batch.is_empty() && events.send(batch).await.is_err() {
            // Receiver gone; nothing useful to do.
        }
        Ok(())
    }
}

/// Strip `prefix` from the start of `name`, case-insensitively. The prefix
/// matches with or without a trailing underscore. Returns the remainder
/// (without leading underscores).
fn strip_prefix_ci<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    if !name
        .as_bytes()
        .iter()
        .zip(prefix.as_bytes())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return None;
    }
    if name.len() < prefix.len() {
        return None;
    }
    let rest = &name[prefix.len()..];
    Some(rest.trim_start_matches('_'))
}

/// Translate an env-var name (post-prefix-strip) into a [`Path`]. Splits on
/// `__`, lowercases each segment, and converts numeric segments to
/// [`Value::Int`].
fn key_to_path(key: &str) -> Path {
    let mut path = Path::new();
    for segment in key.split("__") {
        if segment.is_empty() {
            continue;
        }
        let lowered = segment.to_ascii_lowercase();
        if let Ok(i) = lowered.parse::<i64>() {
            path.push(Value::Int(i));
        } else {
            path.push(Value::String(lowered.into()));
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigHandle;
    use std::sync::Arc;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn no_prefix_emits_all() {
        let provider: Arc<dyn ConfigProvider> = Arc::new(EnvProvider::from_vars(
            None,
            vars(&[("APP__NAME", "frances")]),
        ));
        let handle = ConfigHandle::build(vec![provider]).await.unwrap();
        assert_eq!(
            handle.snapshot().get("app::name").value(),
            Some(&Value::String("frances".into()))
        );
    }

    #[tokio::test]
    async fn prefix_filters_and_strips() {
        let provider: Arc<dyn ConfigProvider> = Arc::new(EnvProvider::from_vars(
            Some("MYAPP".into()),
            vars(&[("MYAPP__DATABASE__HOST", "localhost"), ("OTHER", "ignored")]),
        ));
        let handle = ConfigHandle::build(vec![provider]).await.unwrap();
        assert_eq!(
            handle.snapshot().get("database::host").value(),
            Some(&Value::String("localhost".into()))
        );
        assert!(handle.snapshot().get("other").value().is_none());
    }

    #[tokio::test]
    async fn numeric_segments_become_int() {
        let provider: Arc<dyn ConfigProvider> = Arc::new(EnvProvider::from_vars(
            None,
            vars(&[("TAGS__0", "alpha"), ("TAGS__1", "beta")]),
        ));
        let handle = ConfigHandle::build(vec![provider]).await.unwrap();
        let snap = handle.snapshot();
        assert_eq!(
            snap.get(Path::from(vec![
                Value::String("tags".into()),
                Value::Int(0),
            ]))
            .value(),
            Some(&Value::String("alpha".into()))
        );
    }
}
