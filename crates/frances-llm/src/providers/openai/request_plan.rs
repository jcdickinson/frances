use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;

use frances_config::EnvLookup;
use serde_json::Value;
use thiserror::Error as ThisError;
use url::Url;

use crate::config::{AuthMethod, ModelConfig, ProviderConfig, ResponsesModelExtras};

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("invalid base_url: {0}")]
    JoinBaseUrl(#[source] url::ParseError),
    #[error("env var '{0}' not set in client environment")]
    MissingEnvVar(String),
    #[error("env var '{var}' not set in client environment — {hint}")]
    MissingEnvVarHinted { var: String, hint: String },
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
    #[error("parse extra_completion_properties as JSON: {0}")]
    ParseExtras(#[source] serde_json::Error),
    #[error("extra_completion_properties must be a JSON object, got {0}")]
    ExtrasNotObject(&'static str),
}

pub(super) struct RequestPlan {
    pub(super) url: Url,
    pub(super) bearer_token: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) model: ModelConfig,
    pub(super) extra_completion_properties: Option<String>,
}

impl RequestPlan {
    pub(super) fn build(
        provider_config: &ProviderConfig,
        extras: &ResponsesModelExtras,
        model: &ModelConfig,
        env: &HashMap<OsString, OsString>,
    ) -> Result<Self, Error> {
        let bearer_token = resolve_bearer(&provider_config.auth, env)?;
        let url = provider_config
            .base_url
            .join("chat/completions")
            .map_err(Error::JoinBaseUrl)?;
        let headers = expand_headers(&provider_config.http_headers, env)?;
        Ok(RequestPlan {
            url,
            bearer_token,
            headers,
            extra_completion_properties: extras.extra_completion_properties.clone(),
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
            .ok_or_else(|| match env_key_instructions {
                Some(hint) => Error::MissingEnvVarHinted {
                    var: env_key.clone(),
                    hint: hint.clone(),
                },
                None => Error::MissingEnvVar(env_key.clone()),
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
            tracing::warn!(
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

pub(super) fn merge_extras(body: &mut Value, extras: Option<&str>) -> Result<(), Error> {
    let Some(extras) = extras else {
        return Ok(());
    };
    let parsed: Value = serde_json::from_str(extras).map_err(Error::ParseExtras)?;
    let Value::Object(extras_obj) = parsed else {
        return Err(Error::ExtrasNotObject(type_name_of(&parsed)));
    };
    let Value::Object(body_obj) = body else {
        unreachable!("body is constructed as a JSON object above");
    };
    for (k, v) in extras_obj {
        body_obj.insert(k, v);
    }
    Ok(())
}

fn type_name_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_extras_overrides_existing_keys() {
        let mut body = json!({
            "model": "qwen",
            "max_tokens": 1000,
        });
        merge_extras(
            &mut body,
            Some(r#"{"max_tokens": 2000, "provider": {"order": ["parasail"]}}"#),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 2000);
        assert_eq!(body["provider"]["order"][0], "parasail");
        assert_eq!(body["model"], "qwen");
    }

    #[test]
    fn merge_extras_rejects_non_object() {
        let mut body = json!({});
        let err = merge_extras(&mut body, Some(r#"["nope"]"#)).unwrap_err();
        assert!(matches!(err, Error::ExtrasNotObject(_)));
    }

    #[test]
    fn merge_extras_none_is_noop() {
        let mut body = json!({"a": 1});
        merge_extras(&mut body, None).unwrap();
        assert_eq!(body, json!({"a": 1}));
    }
}
