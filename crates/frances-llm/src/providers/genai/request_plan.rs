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
use tracing::warn;
use url::Url;

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
}

impl RequestPlan {
    pub(super) async fn build(
        provider_config: &ProviderConfig,
        model: &ModelConfig,
        env: &HashMap<OsString, OsString>,
        http: &reqwest::Client,
    ) -> Result<Self, Error> {
        let auth = resolve_auth(&provider_config.auth, env, http).await?;
        let mut extra_headers = expand_headers(&provider_config.http_headers, env)?;
        extra_headers.extend(auth.headers);
        Ok(RequestPlan {
            base_url: provider_config.base_url.clone(),
            api_key: auth.bearer,
            extra_headers,
            model: model.clone(),
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
