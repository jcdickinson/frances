//! Per-request derived state: resolved bearer, base URL, and any
//! user-defined HTTP headers from `provider_config.http_headers`. These
//! feed the genai `Client` build + per-call `ChatOptions`.
//!
//! `Authorization` is always derived from `provider_config.auth`; any
//! `Authorization` entry in `http_headers` is ignored with a warning.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;

use frances_config::EnvLookup;
use thiserror::Error as ThisError;
use tracing::{trace, warn};
use url::Url;

use frances_models_llm::NormalizedEffort;
use frances_models_llm::config::{AuthMethod, ModelConfig, ProviderConfig};

use super::codex_auth;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error(
        "env var '{var}' not set in client environment{}",
        hint.as_deref().map(|h| format!(" — {h}")).unwrap_or_default()
    )]
    MissingEnvVar { var: String, hint: Option<String> },
    #[error("read auth file {path}: {source}")]
    ReadAuthFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command-backed auth is not implemented yet")]
    AuthCommandUnimplemented,
    #[error(transparent)]
    Codex(#[from] codex_auth::Error),
    #[error("expand header {name}: {source}")]
    ExpandHeader {
        name: String,
        #[source]
        source: frances_config::EnvStringExpandError,
    },
}

pub(super) struct RequestPlan {
    pub(super) base_url: Url,
    pub(super) api_key: String,
    /// User-defined HTTP headers (excluding `Authorization`) that
    /// downstream consumers (e.g. `ChatOptions.extra_headers`) can
    /// forward.
    pub(super) extra_headers: Vec<(String, String)>,
    pub(super) model: ModelConfig,
    pub(super) effort_label: Option<String>,
}

impl RequestPlan {
    pub(super) async fn build(
        provider_config: &ProviderConfig,
        model: &ModelConfig,
        effort_override: Option<NormalizedEffort>,
        env: &HashMap<OsString, OsString>,
        http: &reqwest::Client,
    ) -> Result<Self, Error> {
        let auth = resolve_auth(&provider_config.auth, env, http).await?;
        let mut extra_headers = expand_headers(&provider_config.http_headers, env)?;
        extra_headers.extend(auth.headers);
        let effort = effort_override.or(model.effort);
        let effort_label = match (effort, model.effort_tiers.as_ref()) {
            (Some(effort), Some(tiers)) => {
                let label = tiers.label_for(effort).to_owned();
                trace!(
                    model = %model.id,
                    effort = effort.get(),
                    tier = %label,
                    "mapped normalized model effort"
                );
                Some(label)
            }
            (Some(effort), None) => {
                warn!(
                    model = %model.id,
                    effort = effort.get(),
                    "model effort omitted because effort_tiers is not configured"
                );
                None
            }
            (None, _) => None,
        };
        Ok(RequestPlan {
            base_url: provider_config.base_url.clone(),
            api_key: auth.bearer,
            extra_headers,
            model: model.clone(),
            effort_label,
        })
    }
}

/// A resolved bearer plus any headers that must travel with it. Static
/// auth methods carry no extra headers; Codex auth carries the
/// `ChatGPT-Account-ID` it read alongside the token.
struct ResolvedAuth {
    bearer: String,
    headers: Vec<(String, String)>,
}

async fn resolve_auth(
    auth: &AuthMethod,
    env: &HashMap<OsString, OsString>,
    http: &reqwest::Client,
) -> Result<ResolvedAuth, Error> {
    let bearer = match auth {
        AuthMethod::EnvKey {
            env_key,
            env_key_instructions,
        } => env
            .get(std::ffi::OsStr::new(env_key))
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or_else(|| Error::MissingEnvVar {
                var: env_key.clone(),
                hint: env_key_instructions.clone(),
            })?,
        AuthMethod::Token { token } => token.clone(),
        AuthMethod::File { file } => std::fs::read_to_string(file)
            .map(|s| s.trim().to_owned())
            .map_err(|source| Error::ReadAuthFile {
                path: file.clone(),
                source,
            })?,
        AuthMethod::Codex { codex_home, .. } => {
            let creds = codex_auth::resolve(codex_home.as_deref(), env, http).await?;
            let headers = creds
                .account_id
                .map(|id| vec![("ChatGPT-Account-ID".to_string(), id)])
                .unwrap_or_default();
            return Ok(ResolvedAuth {
                bearer: creds.access_token,
                headers,
            });
        }
        AuthMethod::Command { .. } => return Err(Error::AuthCommandUnimplemented),
    };
    Ok(ResolvedAuth {
        bearer,
        headers: Vec::new(),
    })
}

fn expand_headers(
    raw: &BTreeMap<String, frances_config::EnvString>,
    env: &dyn EnvLookup,
) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, template) in raw {
        if name.eq_ignore_ascii_case("authorization") {
            warn!(
                header = %name,
                "Authorization header in http_headers is ignored — auth resolves it"
            );
            continue;
        }
        let value = template.expand(env).map_err(|source| Error::ExpandHeader {
            name: name.clone(),
            source,
        })?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_models_llm::EffortTiers;

    fn provider() -> ProviderConfig {
        ProviderConfig {
            kind: "openai-responses".into(),
            name: None,
            base_url: "https://example.com".parse().unwrap(),
            auth: AuthMethod::Token {
                token: "test".into(),
            },
            http_headers: Default::default(),
            query_params: Default::default(),
            supports_websockets: false,
            request_max_retries: 0,
            stream_max_retries: 0,
            stream_idle_timeout_ms: 1,
        }
    }

    fn model(effort: Option<u8>, effort_tiers: Option<EffortTiers>) -> ModelConfig {
        ModelConfig {
            model_provider: "test".into(),
            id: "test-model".into(),
            max_tokens: None,
            stream_idle_timeout_ms: 1,
            effort: effort.map(|value| NormalizedEffort::new(value).unwrap()),
            effort_tiers,
            service_tier: None,
        }
    }

    #[tokio::test]
    async fn session_effort_overrides_model_default() {
        let plan = RequestPlan::build(
            &provider(),
            &model(Some(25), Some(EffortTiers::openai())),
            Some(NormalizedEffort::new(100).unwrap()),
            &HashMap::new(),
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        assert_eq!(plan.effort_label.as_deref(), Some("max"));
    }

    #[tokio::test]
    async fn model_default_applies_without_session_override() {
        let plan = RequestPlan::build(
            &provider(),
            &model(Some(50), Some(EffortTiers::openai())),
            None,
            &HashMap::new(),
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        assert_eq!(plan.effort_label.as_deref(), Some("medium"));
    }

    #[tokio::test]
    async fn effort_without_tiers_is_omitted() {
        let plan = RequestPlan::build(
            &provider(),
            &model(Some(100), None),
            None,
            &HashMap::new(),
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        assert_eq!(plan.effort_label, None);
    }
}
