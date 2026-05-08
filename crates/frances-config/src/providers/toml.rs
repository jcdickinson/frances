use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::SourceLoadError;
use crate::event::{ConfigEvent, EventSender};
use crate::provider::{ConfigProvider, ProviderError};
use crate::value::{Path, Value};

/// Reads config from a TOML file.
///
/// TOML scalars map: bool→`Bool`, integer→`Int`, float→`Float`,
/// string/datetime→`String`. Tables produce `String` segments; arrays
/// produce `Int` index segments.
pub struct TomlProvider {
    path: PathBuf,
    optional: bool,
}

impl TomlProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            optional: false,
        }
    }

    /// Don't error if the file is missing.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }

    async fn read_to_string(&self) -> Result<Option<String>, ProviderError> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if self.optional && e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ProviderError::from(
                SourceLoadError::<toml::de::Error>::Read {
                    name: self.name(),
                    source: e,
                },
            )),
        }
    }
}

#[async_trait]
impl ConfigProvider for TomlProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        let Some(contents) = self.read_to_string().await? else {
            return Ok(());
        };
        let value: toml::Value = toml::from_str(&contents).map_err(|e| {
            ProviderError::from(SourceLoadError::Parse {
                name: self.name(),
                source: e,
            })
        })?;
        let mut emitter = Emitter { events: &events };
        emitter.walk(Path::new(), value).await?;
        Ok(())
    }
}

struct Emitter<'a> {
    events: &'a EventSender,
}

impl<'a> Emitter<'a> {
    fn walk<'s>(
        &'s mut self,
        path: Path,
        value: toml::Value,
    ) -> futures::future::BoxFuture<'s, Result<(), ProviderError>> {
        Box::pin(async move {
            match value {
                toml::Value::Table(t) => {
                    for (k, v) in t {
                        let mut child_path = path.clone();
                        child_path.push(Value::String(k.into()));
                        self.walk(child_path, v).await?;
                    }
                }
                toml::Value::Array(arr) => {
                    for (i, v) in arr.into_iter().enumerate() {
                        let mut child_path = path.clone();
                        child_path.push(Value::Int(i as i64));
                        self.walk(child_path, v).await?;
                    }
                }
                leaf => {
                    let value = toml_leaf_to_value(leaf);
                    if self
                        .events
                        .send(ConfigEvent::new(path, value))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    }
}

fn toml_leaf_to_value(v: toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.into()),
        toml::Value::Integer(i) => Value::Int(i),
        toml::Value::Float(f) => Value::Float(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string().into()),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            unreachable!("walk handles compound values directly")
        }
    }
}
