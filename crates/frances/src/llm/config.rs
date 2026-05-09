use std::collections::BTreeMap;
use std::path::PathBuf;

use frances_config::EnvString;
use serde::Deserialize;
use url::Url;

/// Codex-shaped provider definition. One instance per `model_providers.<id>`
/// table in the config tree. The `id` itself is the binding key, not a field.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[expect(
        dead_code,
        reason = "human-facing display only; not yet surfaced in the TUI"
    )]
    #[serde(default)]
    pub name: Option<String>,
    pub base_url: Url,
    pub auth: AuthMethod,
    #[serde(default)]
    pub http_headers: BTreeMap<String, EnvString>,
    #[expect(
        dead_code,
        reason = "applied at request time; wired up after auth lands"
    )]
    #[serde(default)]
    pub query_params: BTreeMap<String, EnvString>,
    #[expect(
        dead_code,
        reason = "only one variant exists today; consulted when a second wire ships"
    )]
    #[serde(default)]
    pub wire_api: WireApi,
    #[expect(dead_code, reason = "WebSocket transport is a follow-up")]
    #[serde(default)]
    pub supports_websockets: bool,
    #[expect(dead_code, reason = "retry policy not enforced this pass")]
    #[serde(default = "default_request_max_retries")]
    pub request_max_retries: u32,
    #[expect(dead_code, reason = "retry policy not enforced this pass")]
    #[serde(default = "default_stream_max_retries")]
    pub stream_max_retries: u32,
    #[expect(
        dead_code,
        reason = "per-request stream timeout currently sourced from the model"
    )]
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
}

/// Vendor-neutral name for the wire protocol the provider speaks. Today
/// the only variant is `Responses` (OpenAI-style chat completions); a
/// future variant will slot in here without breaking existing config.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    #[default]
    Responses,
}

/// Untagged: serde walks the variants top-to-bottom and picks the first
/// whose required fields are present. Order from most specific (table
/// form, `auth.command`) to least (single string, `token`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum AuthMethod {
    Command {
        #[expect(dead_code, reason = "command auth is parsed but not yet implemented")]
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

#[derive(Debug, Clone, Deserialize)]
pub struct AuthCommand {
    #[expect(dead_code, reason = "command auth not implemented this pass")]
    pub command: String,
    #[expect(dead_code, reason = "command auth not implemented this pass")]
    #[serde(default)]
    pub args: Vec<String>,
    #[expect(dead_code, reason = "command auth not implemented this pass")]
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[expect(dead_code, reason = "command auth not implemented this pass")]
    #[serde(default = "default_refresh_interval_ms")]
    pub refresh_interval_ms: u64,
    #[expect(dead_code, reason = "command auth not implemented this pass")]
    #[serde(default = "default_auth_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub model_provider: String,
    pub id: String,
    #[serde(default = "default_model_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_model_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[expect(
        dead_code,
        reason = "0..=100 → provider-specific scale not implemented yet"
    )]
    #[serde(default)]
    pub reasoning_effort: Option<u8>,
    #[expect(
        dead_code,
        reason = "0..=100 → provider-specific scale not implemented yet"
    )]
    #[serde(default)]
    pub service_tier: Option<u8>,
}

/// Wire-specific extras keyed by the wire model id. The Responses-API
/// client reads its own `responses_models::<id>` binding so the
/// universal `ModelConfig` stays clean. Providers using a different wire
/// would have their own `<wire>_models::<id>` table.
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
fn default_model_max_tokens() -> u32 {
    1_000
}
fn default_model_stream_idle_timeout_ms() -> u64 {
    120_000
}
