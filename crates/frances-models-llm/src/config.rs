use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use frances_config::EnvString;
use serde::Deserialize;
use url::Url;

/// Codex-shaped provider definition. One instance per `model_providers.<id>`
/// table in the config tree. The `id` itself is the binding key, not a field.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Wire-name selector. Identifies which provider wire shape the
    /// `Provider` impl should drive — e.g. `"openai-chat"`,
    /// `"openai-responses"`, `"anthropic"`, `"gemini"`, `"openrouter"`,
    /// `"zai"`, `"moonshot"`, `"deepseek"`, `"ollama"`. Validated at
    /// provider-build time and surfaces as `Provider::kind()`. Required.
    pub kind: String,
    /// Human-facing display name.
    #[serde(default)]
    pub name: Option<String>,
    pub base_url: Url,
    pub auth: AuthMethod,
    #[serde(default)]
    pub http_headers: BTreeMap<String, EnvString>,
    /// Applied at request time; wired up after auth lands.
    #[serde(default)]
    pub query_params: BTreeMap<String, EnvString>,
    #[serde(default)]
    pub supports_websockets: bool,
    #[serde(default = "default_request_max_retries")]
    pub request_max_retries: u32,
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
    /// 0..=100 → mapped to the provider's reasoning-effort scale at request
    /// time. The OpenRouter Responses provider maps this onto the
    /// `low`/`medium`/`high` effort enum.
    #[serde(default)]
    pub reasoning_effort: Option<u8>,
    /// 0..=100 → mapped to a provider service-tier label at request time.
    /// The OpenRouter Responses provider maps to `flex`/`default`/`priority`.
    #[serde(default)]
    pub service_tier: Option<u8>,
}

/// One entry under `[openrouter.models.<name>]`. Keyed by the same
/// model name as the corresponding `[models.<name>]` binding.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterModelConfig {
    /// Repair non-scalar tool-call args that arrived as JSON-encoded
    /// strings. See `docs/todo/qwen-tool-arg-repair.md` for the
    /// upstream quirk this works around.
    #[serde(default)]
    pub qwen_quirks: bool,
}

/// Top-level `[openrouter]` config block. Bound by the genai provider
/// at construction time when its `kind` is `openrouter`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterConfig {
    #[serde(default)]
    pub models: HashMap<String, OpenRouterModelConfig>,
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
