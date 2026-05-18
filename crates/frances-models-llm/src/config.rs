use std::collections::BTreeMap;
use std::path::PathBuf;

use frances_config::EnvString;
use serde::Deserialize;
use url::Url;

/// Codex-shaped provider definition. One instance per `model_providers.<id>`
/// table in the config tree. The `id` itself is the binding key, not a field.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Human-facing display name; not yet surfaced in the TUI.
    #[serde(default)]
    pub name: Option<String>,
    pub base_url: Url,
    pub auth: AuthMethod,
    #[serde(default)]
    pub http_headers: BTreeMap<String, EnvString>,
    /// Applied at request time; wired up after auth lands.
    #[serde(default)]
    pub query_params: BTreeMap<String, EnvString>,
    /// WebSocket transport is a follow-up.
    #[serde(default)]
    pub supports_websockets: bool,
    /// Retry policy not enforced this pass.
    #[serde(default = "default_request_max_retries")]
    pub request_max_retries: u32,
    /// Retry policy not enforced this pass.
    #[serde(default = "default_stream_max_retries")]
    pub stream_max_retries: u32,
    /// Per-request stream timeout currently sourced from the model.
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
}

/// Untagged: serde walks the variants top-to-bottom and picks the first
/// whose required fields are present. Order from most specific (table
/// form, `auth.command`) to least (single string, `token`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum AuthMethod {
    /// Command auth is parsed but not yet implemented.
    Command {
        command: AuthCommand,
    },
    EnvKey {
        env_key: String,
        #[serde(default)]
        env_key_instructions: Option<String>,
    },
    File {
        file: PathBuf,
    },
    Token {
        token: String,
    },
}

/// Command auth not implemented this pass — fields parse and round-trip,
/// runtime side is a follow-up.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_refresh_interval_ms")]
    pub refresh_interval_ms: u64,
    #[serde(default = "default_auth_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub model_provider: String,
    pub id: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_model_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    /// 0..=100 → provider-specific scale not implemented yet.
    #[serde(default)]
    pub reasoning_effort: Option<u8>,
    /// 0..=100 → provider-specific scale not implemented yet.
    #[serde(default)]
    pub service_tier: Option<u8>,
}

/// Wire-specific extras for the OpenAI chat-completions wire. Bound by
/// the `ProviderCache` at `model_provider_extensions::<provider_id>` and
/// passed by value to the provider impl's `new` as its associated
/// `Extras` type. Other wires' implementations carry their own extras
/// type; the cache deserialises whichever shape the impl declares.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponsesModelExtras {
    /// JSON-encoded object. Shallow-merged into the chat-completion body
    /// at request time; keys here override our built-ins. Intended for
    /// vendor-specific knobs (e.g. OpenRouter's
    /// `{"provider": {"order": [...]}}`).
    #[serde(default)]
    pub extra_completion_properties: Option<String>,
}

fn default_request_max_retries() -> u32 {
    4
}
fn default_stream_max_retries() -> u32 {
    5
}
fn default_stream_idle_timeout_ms() -> u64 {
    300_000
}
fn default_refresh_interval_ms() -> u64 {
    300_000
}
fn default_auth_timeout_ms() -> u64 {
    5_000
}
fn default_model_stream_idle_timeout_ms() -> u64 {
    120_000
}
