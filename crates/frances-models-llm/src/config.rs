use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use frances_config::EnvString;
use serde::Deserialize;
use url::Url;

use crate::{EffortTiers, NormalizedEffort};

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
    /// Reuse the credentials the OpenAI Codex CLI wrote via `codex login`.
    /// Selected by `auth = { codex = true }`; the access token is read
    /// from `auth.json` and refreshed on demand. `codex_home` overrides
    /// the credential directory (default `$CODEX_HOME`, then `~/.codex`).
    Codex {
        codex: CodexEnabled,
        #[serde(default)]
        codex_home: Option<PathBuf>,
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

/// Discriminator for [`AuthMethod::Codex`]. Deserializes only from the
/// literal `true`, so `auth = { codex = false }` is a parse error rather
/// than a representable "codex auth, but off" no-op.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "bool")]
pub struct CodexEnabled;

impl TryFrom<bool> for CodexEnabled {
    type Error = &'static str;

    fn try_from(value: bool) -> Result<Self, Self::Error> {
        if value {
            Ok(CodexEnabled)
        } else {
            Err("codex auth marker must be `true`")
        }
    }
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
    /// Provider-neutral default effort percentage.
    #[serde(default)]
    pub effort: Option<NormalizedEffort>,
    /// Ordered provider labels used to map normalized effort percentages.
    #[serde(default)]
    pub effort_tiers: Option<EffortTiers>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_true_selects_codex_variant() {
        let auth: AuthMethod =
            serde_json::from_value(serde_json::json!({ "codex": true })).unwrap();
        assert!(matches!(
            auth,
            AuthMethod::Codex {
                codex_home: None,
                ..
            }
        ));
    }

    #[test]
    fn codex_with_home_override() {
        let auth: AuthMethod = serde_json::from_value(
            serde_json::json!({ "codex": true, "codex_home": "/tmp/codex" }),
        )
        .unwrap();
        let AuthMethod::Codex { codex_home, .. } = auth else {
            panic!("expected codex variant");
        };
        assert_eq!(codex_home, Some(PathBuf::from("/tmp/codex")));
    }

    #[test]
    fn codex_false_is_a_parse_error() {
        let err = serde_json::from_value::<AuthMethod>(serde_json::json!({ "codex": false }));
        assert!(err.is_err(), "codex = false must not deserialize");
    }

    #[test]
    fn token_string_still_parses() {
        let auth: AuthMethod =
            serde_json::from_value(serde_json::json!({ "token": "secret" })).unwrap();
        assert!(matches!(auth, AuthMethod::Token { token } if token == "secret"));
    }

    #[test]
    fn model_effort_and_openai_tiers_deserialize() {
        let model: ModelConfig = serde_json::from_value(serde_json::json!({
            "model_provider": "openai",
            "id": "gpt",
            "effort": 50,
            "effort_tiers": "openai"
        }))
        .unwrap();
        assert_eq!(model.effort.map(|effort| effort.get()), Some(50));
        assert_eq!(model.effort_tiers.unwrap(), EffortTiers::openai());
    }

    #[test]
    fn model_effort_configuration_rejects_invalid_values() {
        let base = serde_json::json!({
            "model_provider": "openai",
            "id": "gpt"
        });
        for invalid in [
            serde_json::json!(101),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("high"),
        ] {
            let mut value = base.clone();
            value["effort"] = invalid;
            assert!(serde_json::from_value::<ModelConfig>(value).is_err());
        }
    }
}
