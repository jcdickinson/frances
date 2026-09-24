use std::{collections::BTreeMap, sync::Arc};

use frances_harness::{Output, Outputs, PermissionRequest, PermissionResponse};
use frances_mcp::{Config, Connection, Environment, Selection};
use frances_models_llm::{ToolCall, ToolDef, ToolFunction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP server {server}: {source}")]
    Server {
        server: String,
        #[source]
        source: frances_mcp::Error,
    },
    #[error(transparent)]
    Config(#[from] frances_mcp::Error),
    #[error("invalid MCP arguments: {0}")]
    Arguments(String),
    #[error("MCP permission denied")]
    Denied,
    #[error("MCP tool inventory changed; apply the MCP selection again to refresh the context")]
    Changed,
    #[error("MCP operation interrupted")]
    Interrupted,
    #[error("agent input closed")]
    Closed,
    #[error("MCP result: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, specta::Type)]
pub struct Status {
    pub selection: Selection,
    pub presets: BTreeMap<String, Vec<String>>,
    pub available_servers: Vec<String>,
    pub active_servers: Vec<String>,
}

enum Operation {
    Tool(Box<frances_mcp::Tool>),
    Resources,
    ReadResource,
}

struct Entry {
    server: String,
    operation: Operation,
    definition: ToolDef,
    revision: u64,
}

#[derive(Default)]
pub(super) struct McpTools {
    pub connections: BTreeMap<String, Arc<Connection>>,
    entries: BTreeMap<String, Entry>,
}

impl McpTools {
    pub async fn select(
        &self,
        config: &Config,
        selection: &Selection,
        env: &Environment,
        cancel: &CancellationToken,
    ) -> Result<Self, McpError> {
        let names = config.resolve(selection)?;
        let mut next = Self::default();
        for name in names {
            let connection = match self.connections.get(&name) {
                Some(connection) if !connection.is_closed() => connection.clone(),
                _ => Arc::new(
                    Connection::connect(name.clone(), &config.servers[&name], env, cancel)
                        .await
                        .map_err(|source| McpError::Server {
                            server: name.clone(),
                            source,
                        })?,
                ),
            };
            let revision = connection.catalog_revision();
            let tools = connection
                .tools(cancel)
                .await
                .map_err(|source| McpError::Server {
                    server: name.clone(),
                    source,
                })?;
            if revision != connection.catalog_revision() {
                return Err(McpError::Changed);
            }
            for tool in &tools {
                let alias = tool_name(&name, "tool", &tool.name);
                next.entries.insert(
                    alias.clone(),
                    Entry {
                        server: name.clone(),
                        revision,
                        operation: Operation::Tool(Box::new(tool.clone())),
                        definition: ToolDef::Function(ToolFunction {
                            name: alias,
                            description: format!(
                                "MCP server {name}, tool {}. {}",
                                tool.name,
                                tool.description.as_deref().unwrap_or_default()
                            ),
                            parameters: Value::Object((*tool.input_schema).clone()),
                        }),
                    },
                );
            }
            if connection.has_resources() {
                for (kind, operation, description, parameters) in [
                    (
                        "resources",
                        Operation::Resources,
                        "List resources and URI templates",
                        json!({"type":"object","properties":{},"additionalProperties":false}),
                    ),
                    (
                        "read_resource",
                        Operation::ReadResource,
                        "Read a resource by URI",
                        json!({"type":"object","properties":{"uri":{"type":"string"}},"required":["uri"],"additionalProperties":false}),
                    ),
                ] {
                    let alias = tool_name(&name, "host", kind);
                    next.entries.insert(
                        alias.clone(),
                        Entry {
                            server: name.clone(),
                            revision,
                            operation,
                            definition: ToolDef::Function(ToolFunction {
                                name: alias,
                                description: format!("{description} from MCP server {name}."),
                                parameters,
                            }),
                        },
                    );
                }
            }
            next.connections.insert(name, connection);
        }
        Ok(next)
    }

    pub fn status(&self, config: &Config, selection: Selection) -> Status {
        Status {
            selection,
            presets: config
                .presets
                .iter()
                .map(|(name, preset)| (name.clone(), preset.servers.clone()))
                .collect(),
            available_servers: config.servers.keys().cloned().collect(),
            active_servers: self.connections.keys().cloned().collect(),
        }
    }

    pub fn definitions(&self) -> impl Iterator<Item = ToolDef> + '_ {
        self.entries.values().map(|entry| entry.definition.clone())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn instructions(&self) -> String {
        self.connections
            .values()
            .filter_map(|c| {
                c.instructions()
                    .map(|text| format!("\n\nMCP server {} instructions:\n{text}", c.name))
            })
            .collect()
    }

    pub async fn execute(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
        output: &Outputs,
    ) -> Result<(String, bool), McpError> {
        if cancel.is_cancelled() {
            return Err(McpError::Interrupted);
        }
        let entry = &self.entries[&call.name];
        let connection = &self.connections[&entry.server];
        if connection.catalog_revision() != entry.revision {
            return Err(McpError::Changed);
        }
        let ToolDef::Function(definition) = &entry.definition;
        frances_models_llm::tool_args::validate(&call.arguments, &definition.parameters)
            .map_err(|error| McpError::Arguments(error.0))?;
        if let Some(error) = &call.error {
            return Err(McpError::Arguments(error.message.clone()));
        }
        let args = call
            .arguments
            .as_object()
            .ok_or_else(|| McpError::Arguments("expected an object".into()))?;
        let operation = match &entry.operation {
            Operation::Tool(tool) => tool.name.as_ref(),
            Operation::Resources => "resources/list",
            Operation::ReadResource => "resources/read",
        };
        if !matches!(entry.operation, Operation::Resources) {
            let (reply, response) = oneshot::channel();
            output.send(Output::Permission(PermissionRequest {
                prompt: format!(
                    "Allow MCP server {} to call {operation}?\n{}",
                    entry.server, call.arguments
                ),
                tool_call: Some(call.clone()),
                allow_auto: false,
                reply,
            }));
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(McpError::Interrupted),
                response = response => match response {
                    Ok(PermissionResponse::Yes { .. }) => {},
                    Ok(PermissionResponse::No { .. }) => return Err(McpError::Denied),
                    Err(error) => { tracing::debug!(%error, "MCP permission responder closed"); return Err(McpError::Denied); },
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(McpError::Interrupted);
        }
        if connection.catalog_revision() != entry.revision {
            return Err(McpError::Changed);
        }
        let declaration = match &entry.operation {
            Operation::Tool(tool) => Some(tool.as_ref()),
            _ => None,
        };
        let snapshot = json!({"server":entry.server,"method":operation,"arguments":call.arguments,"declaration":declaration});
        let id = output.open("mcp", snapshot.clone());
        let result = async {
            let (result, content, is_error) = match &entry.operation {
                Operation::Tool(tool) => {
                    let result = connection
                        .call(tool.name.to_string(), args.clone(), cancel)
                        .await?;
                    let is_error = result.is_error.unwrap_or(false);
                    let content = super::mcp_content::tool(&result);
                    (serde_json::to_value(result)?, content, is_error)
                }
                Operation::Resources => {
                    let resources = connection.resources(cancel).await?;
                    let templates = connection.templates(cancel).await?;
                    let content = super::mcp_content::catalog(&resources, &templates);
                    (
                        json!({"resources":resources,"templates":templates}),
                        content,
                        false,
                    )
                }
                Operation::ReadResource => {
                    let result = connection
                        .read(args["uri"].as_str().expect("validated URI").into(), cancel)
                        .await?;
                    let content = super::mcp_content::resources(&result);
                    (serde_json::to_value(result)?, content, false)
                }
            };
            Ok::<_, frances_mcp::Error>((result, content, is_error))
        }
        .await;
        match result {
            Ok((result, content, is_error)) => {
                output.settle(id, json!({"server":entry.server,"method":operation,"arguments":call.arguments,"result":result,"isError":is_error,"declaration":declaration}));
                Ok((content, is_error))
            }
            Err(source) => {
                output.settle(id, json!({"server":entry.server,"method":operation,"arguments":call.arguments,"error":source.to_string(),"declaration":declaration}));
                Err(McpError::Server {
                    server: entry.server.clone(),
                    source,
                })
            }
        }
    }

    pub async fn close(self) {
        for connection in self.connections.into_values() {
            if let Ok(connection) = Arc::try_unwrap(connection) {
                connection.close().await;
            }
        }
    }
}

/// Stable, provider-safe names; identity includes the server and operation kind.
pub fn tool_name(server: &str, kind: &str, name: &str) -> String {
    let identity = format!("{server}\0{kind}\0{name}");
    let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes());
    let readable: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(24)
        .collect();
    format!("mcp_{readable}_{}", id.simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aliases_distinguish_servers_and_host_operations() {
        assert_ne!(
            tool_name("a", "tool", "read"),
            tool_name("b", "tool", "read")
        );
        assert_ne!(
            tool_name("a", "tool", "read"),
            tool_name("a", "host", "read")
        );
        let name = tool_name("long server", "tool", &"🦀tool / name".repeat(100));
        assert!(name.len() <= 64);
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    }
}
