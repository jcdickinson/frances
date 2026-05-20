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
    /// forward. Empty if the config sets none.
    pub(super) extra_headers: Vec<(String, String)>,
    pub(super) model: ModelConfig,
}

impl RequestPlan {
    pub(super) fn build(
        provider_config: &ProviderConfig,
        model: &ModelConfig,
        env: &HashMap<OsString, OsString>,
    ) -> Result<Self, Error> {
        let api_key = resolve_bearer(&provider_config.auth, env)?;
        let extra_headers = expand_headers(&provider_config.http_headers, env)?;
        Ok(RequestPlan {
            base_url: provider_config.base_url.clone(),
            api_key,
            extra_headers,
            model: model.clone(),
        })
    }
}

fn resolve_bearer(auth: &AuthMethod, env: &HashMap<OsString, OsString>) -> Result<String, Error> {
    match auth {
        AuthMethod::EnvKey {
            env_key,
            env_key_instructions,
        } => env
            .get(std::ffi::OsStr::new(env_key))
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or_else(|| Error::MissingEnvVar {
                var: env_key.clone(),
                hint: env_key_instructions.clone(),
            }),
        AuthMethod::Token { token } => Ok(token.clone()),
        AuthMethod::File { file } => std::fs::read_to_string(file)
            .map(|s| s.trim().to_owned())
            .map_err(|source| Error::ReadAuthFile {
                path: file.clone(),
                source,
            }),
        AuthMethod::Command { .. } => Err(Error::AuthCommandUnimplemented),
    }
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
