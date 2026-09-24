use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use frances_config::{ConfigEvent, ConfigProvider, EventSender, Path, ProviderError, Value};
use serde::Deserialize;
use serde_json::{Map, Value as Json};

use crate::{Config, Selection, ServerConfig};

/// Explicit JSON/JSONC source, adapted into the ordinary MCP config layer.
/// Bad files and entries are logged and skipped, never fatal to host startup.
pub struct McpJsonProvider {
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
enum SourceError {
    #[error("cannot read MCP config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid MCP config {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: jsonc_parser::errors::ParseError,
    },
}

#[derive(Debug, thiserror::Error)]
enum ServerError {
    #[error(transparent)]
    Fields(#[from] serde_json::Error),
    #[error("specify exactly one of command (stdio) or url (HTTP)")]
    AmbiguousTransport,
    #[error("unsupported transport type")]
    UnsupportedTransport,
    #[error("frances/place applies only to stdio; ignoring it on this HTTP server")]
    HttpPlace,
}

#[derive(Deserialize)]
struct FileConfig {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, Json>,
    #[serde(flatten)]
    other: BTreeMap<String, Json>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Place {
    #[default]
    Local,
    Remote,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum TransportType {
    Stdio,
    Http,
    #[serde(other)]
    Unsupported,
}

impl McpJsonProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    async fn read(&self) -> Result<Vec<ConfigEvent>, SourceError> {
        let contents = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|source| SourceError::Read {
                path: self.path.clone(),
                source,
            })?;
        let file: FileConfig = jsonc_parser::parse_to_serde_value(
            &contents,
            &jsonc_parser::ParseOptions {
                allow_comments: true,
                allow_trailing_commas: true,
                allow_loose_object_property_names: false,
                allow_missing_commas: false,
                allow_single_quoted_strings: false,
                allow_hexadecimal_numbers: false,
                allow_unary_plus_numbers: false,
            },
        )
        .map_err(|source| SourceError::Parse {
            path: self.path.clone(),
            source,
        })?;
        for field in file.other.keys().filter(|field| *field != "$schema") {
            tracing::warn!(path = %self.path.display(), field, "ignoring unsupported MCP config field");
        }
        let mut events = Vec::new();
        for (name, server) in file.servers {
            let (native, server) = match self.adapt(&name, server) {
                Ok(server) => server,
                Err(error) => {
                    tracing::error!(%error, path = %self.path.display(), server = name, "skipping invalid MCP server");
                    continue;
                }
            };
            let config = Config {
                servers: BTreeMap::from([(name.clone(), server)]),
                ..Default::default()
            };
            if let Err(error) = config.resolve(&Selection {
                servers: vec![name.clone()],
                ..Default::default()
            }) {
                tracing::error!(%error, path = %self.path.display(), server = name, "skipping invalid MCP server");
                continue;
            }
            collect(Path::from(["mcp", "servers", &name]), native, &mut events);
        }
        Ok(events)
    }

    fn adapt(&self, name: &str, server: Json) -> Result<(Json, ServerConfig), ServerError> {
        let mut fields: Map<String, Json> = serde_json::from_value(server)?;
        let transport_type = match fields.remove("type") {
            Some(value) => serde_json::from_value::<TransportType>(value)?,
            None => match (fields.contains_key("command"), fields.contains_key("url")) {
                (true, false) => TransportType::Stdio,
                (false, true) => TransportType::Http,
                _ => return Err(ServerError::AmbiguousTransport),
            },
        };
        let place = fields.remove("frances/place");
        let (kind, keys): (&str, &[&str]) = match transport_type {
            TransportType::Stdio => {
                let place = place
                    .map(serde_json::from_value::<Place>)
                    .transpose()?
                    .unwrap_or_default();
                let kind = match place {
                    Place::Local => "local-stdio",
                    Place::Remote => "stdio",
                };
                (kind, &["command", "args", "env", "cwd"])
            }
            TransportType::Http => {
                if place.is_some() {
                    let error = ServerError::HttpPlace;
                    tracing::error!(%error, path = %self.path.display(), server = name);
                }
                ("http", &["url", "headers"])
            }
            TransportType::Unsupported => return Err(ServerError::UnsupportedTransport),
        };
        let mut transport = Map::from_iter([("type".into(), Json::String(kind.into()))]);
        for key in keys {
            if let Some(value) = fields.remove(*key) {
                transport.insert((*key).into(), value);
            }
        }
        for field in fields.keys() {
            tracing::warn!(path = %self.path.display(), server = name, field, "ignoring unsupported MCP server field");
        }
        let native = serde_json::json!({"transport": transport});
        let config = serde_json::from_value(native.clone())?;
        Ok((native, config))
    }
}

#[async_trait]
impl ConfigProvider for McpJsonProvider {
    async fn load(&self, events: EventSender) -> Result<(), ProviderError> {
        match self.read().await {
            Ok(batch) => events.send(batch).await.map_err(ProviderError::new),
            Err(error) => {
                tracing::error!(%error, "ignoring MCP config source");
                Ok(())
            }
        }
    }
}

fn collect(path: Path, value: Json, events: &mut Vec<ConfigEvent>) {
    match value {
        Json::Object(fields) => {
            for (name, value) in fields {
                let mut child = path.clone();
                child.push(name);
                collect(child, value, events);
            }
        }
        Json::Array(values) => {
            for (index, value) in values.into_iter().enumerate() {
                let mut child = path.clone();
                child.push(index);
                collect(child, value, events);
            }
        }
        Json::String(value) => events.push(ConfigEvent::new(path, value)),
        Json::Bool(value) => events.push(ConfigEvent::new(path, value)),
        Json::Number(value) => {
            let value = value
                .as_i64()
                .map(Value::Int)
                .unwrap_or_else(|| Value::from(value.to_string()));
            events.push(ConfigEvent::new(path, value));
        }
        Json::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use frances_config::{ConfigHandle, TomlProvider};

    use super::*;
    use crate::Transport;

    async fn load(contents: &str) -> Config {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        std::fs::write(&path, contents).unwrap();
        let handle = ConfigHandle::build(vec![Arc::new(McpJsonProvider::new(path))])
            .await
            .unwrap();
        handle
            .bind::<Config>("mcp")
            .unwrap()
            .get()
            .map(|config| (*config).clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn jsonc_adapts_transports_and_preserves_execution_environment_templates() {
        let config = load(r#"{
            // Common mcpServers convention, with a Frances extension.
            "mcpServers": {
                "local": { "command": "deno", "args": ["run", "server.ts",], "env": {"TOKEN": "${TOKEN}"}, "cwd": "tools", },
                "remote": { "type": "stdio", "frances/place": "remote", "command": "worker-tool" },
                "http": { "url": "http://127.0.0.1:3001/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"} },
            }, /* trailing comma */
        }"#).await;
        assert_eq!(config.servers.len(), 3);
        let Transport::LocalStdio {
            command,
            args,
            env,
            cwd,
        } = &config.servers["local"].transport
        else {
            panic!("expected local stdio")
        };
        assert_eq!(command, "deno");
        assert_eq!(args, &["run", "server.ts"]);
        assert_eq!(env["TOKEN"].raw(), "${TOKEN}");
        assert_eq!(cwd.as_deref(), Some(std::path::Path::new("tools")));
        assert!(matches!(
            config.servers["remote"].transport,
            Transport::Stdio { .. }
        ));
        let Transport::Http { headers, .. } = &config.servers["http"].transport else {
            panic!("expected HTTP")
        };
        assert_eq!(headers["Authorization"].raw(), "Bearer ${TOKEN}");
        assert!(config.resolve(&Selection::default()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn shared_fields_and_http_place_do_not_break_servers() {
        let config = load(r#"{
            "$schema": "https://example.com/schema.json",
            "otherTool": {"setting": true},
            "mcpServers": {
                "local": {"command": "tool", "place": "remote", "autoApprove": ["read"], "another/extension": true},
                "http": {"type": "http", "url": "https://example.com/mcp", "frances/place": {"even": "invalid"}, "disabledTools": []}
            }
        }"#).await;
        assert_eq!(config.servers.len(), 2);
        assert!(matches!(
            config.servers["local"].transport,
            Transport::LocalStdio { .. }
        ));
        assert!(matches!(
            config.servers["http"].transport,
            Transport::Http { .. }
        ));
    }

    #[tokio::test]
    async fn invalid_servers_are_skipped_independently() {
        let config = load(
            r#"{"mcpServers": {
            "good": {"command": "tool"},
            "bad-command": {"command": 42},
            "bad-url": {"url": "not a URL"},
            "bad-place": {"command": "tool", "frances/place": "somewhere"},
            "unsupported": {"type": "sse", "url": "https://example.com/sse"},
            "ambiguous": {"command": "tool", "url": "https://example.com/mcp"},
            "bad.name": {"command": "tool"},
            "unsafe-url": {"url": "https://user:pass@example.com/mcp"}
        }}"#,
        )
        .await;
        assert_eq!(config.servers.keys().collect::<Vec<_>>(), [&"good"]);
    }

    #[tokio::test]
    async fn malformed_or_missing_sources_are_nonfatal_and_jsonc_stays_strict() {
        for text in [
            "broken",
            "",
            "{}",
            "{mcpServers: {}}",
            "{'mcpServers': {}}",
            r#"{"mcpServers":{"a":{"command":"a" "args":[]}}}"#,
        ] {
            assert!(load(text).await.servers.is_empty(), "accepted {text}");
        }
        let temp = tempfile::tempdir().unwrap();
        let handle = ConfigHandle::build(vec![Arc::new(McpJsonProvider::new(
            temp.path().join("missing.json"),
        ))])
        .await
        .unwrap();
        assert!(handle.bind::<Config>("mcp").unwrap().get().is_none());
    }

    #[tokio::test]
    async fn source_layers_with_native_config_without_touching_presets() {
        let temp = tempfile::tempdir().unwrap();
        let toml = temp.path().join("config.toml");
        let json = temp.path().join("mcp.json");
        std::fs::write(
            &toml,
            r#"
            [mcp.servers.existing]
            transport = { type = "local-stdio", command = "old" }
            [mcp.presets.dev]
            servers = ["existing", "added"]
        "#,
        )
        .unwrap();
        std::fs::write(&json, r#"{"mcpServers":{"existing":{"command":"new"},"added":{"url":"https://example.com/mcp"}}}"#).unwrap();
        let handle = ConfigHandle::build(vec![
            Arc::new(TomlProvider::new(toml)),
            Arc::new(McpJsonProvider::new(json)),
        ])
        .await
        .unwrap();
        let binding = handle.bind::<Config>("mcp").unwrap();
        let config = binding.get().unwrap();
        assert_eq!(
            config
                .resolve(&Selection {
                    presets: vec!["dev".into()],
                    ..Default::default()
                })
                .unwrap(),
            ["added", "existing"]
        );
        let Transport::LocalStdio { command, .. } = &config.servers["existing"].transport else {
            panic!("expected local stdio")
        };
        assert_eq!(command, "new");
    }
}
