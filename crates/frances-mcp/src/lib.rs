//! MCP connections and inventories. The host owns selection, permissions, model
//! context lifetime, and any future Frances extension authority.
mod config;
mod connection;
pub use config::{Config, Lifecycle, Preset, Selection, ServerConfig, Transport};
pub use connection::{Connection, Environment};
pub use rmcp::model::{
    CallToolResult, ContentBlock, GetPromptResult, Prompt, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Tool,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("stdio MCP requires a workspace worker; use local-stdio to run on the Frances host")]
    WorkerUnavailable,
    #[error("MCP worker process: {0}")]
    Worker(#[from] frances_worker::ClientError),
    #[error("unknown MCP preset: {0}")]
    UnknownPreset(String),
    #[error("unknown MCP server: {0}")]
    UnknownServer(String),
    #[error("MCP server name must contain only letters, digits, hyphens, or underscores: {0}")]
    InvalidServerName(String),
    #[error("MCP timeouts must be positive: {0}")]
    InvalidTimeout(String),
    #[error("MCP server {0} needs an HTTP(S) URL without credentials or a fragment")]
    InvalidUrl(String),
    #[error("MCP process: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot find MCP executable {command}: {source}")]
    Executable {
        command: String,
        #[source]
        source: frances_core::which::WhichError,
    },
    #[error("MCP startup: {0}")]
    Initialize(#[source] Box<rmcp::service::ClientInitializeError>),
    #[error("MCP request: {0}")]
    Service(#[from] rmcp::service::ServiceError),
    #[error("MCP operation timed out")]
    Timeout,
    #[error("MCP operation interrupted; any remote effects may already have occurred")]
    Interrupted,
    #[error("MCP environment variable is unavailable: {0}")]
    Environment(String),
    #[error("invalid MCP HTTP header name: {0}")]
    HeaderName(#[from] reqwest::header::InvalidHeaderName),
    #[error("invalid MCP HTTP header value")]
    HeaderValue(#[from] reqwest::header::InvalidHeaderValue),
    #[error("MCP HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MCP serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid input schema for MCP tool {tool}: {source}")]
    Schema {
        tool: String,
        #[source]
        source: Box<jsonschema::ValidationError<'static>>,
    },
    #[error("MCP tool {0} must have an object input schema")]
    SchemaNotObject(String),
    #[error("duplicate tool name from MCP server: {0}")]
    DuplicateTool(String),
    #[error("MCP response channel closed: {0}")]
    ResponseClosed(#[from] tokio::sync::oneshot::error::RecvError),
    #[error("MCP server returned an unexpected response")]
    UnexpectedResponse,
    #[error("MCP server requested a client capability that Frances did not advertise")]
    UnsupportedInput,
    #[error("MCP request exceeded the continuation limit")]
    ContinuationLimit,
}

impl From<rmcp::service::ClientInitializeError> for Error {
    fn from(error: rmcp::service::ClientInitializeError) -> Self {
        Self::Initialize(Box::new(error))
    }
}
