use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use frances_config::EnvLookup;
use futures::future::BoxFuture;
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, trace};
use url::Url;

use crate::llm::config::{AuthMethod, ModelConfig, ProviderConfig, ResponsesModelExtras};
use crate::llm::responses::{
    CompletionOutcome, ToolCallAccumulator, ToolChoice, ToolDef, chunk_text_deltas,
    chunk_tool_call_deltas,
};

/// All non-callback inputs to a single chat call, shared by `stream` and
/// `complete`. `session_id` is opaque to the provider — used only as a
/// cache-scoping hint (e.g. Anthropic prompt-cache breakpoints, OpenAI's
/// automatic cache key). Implementations that don't support token caching
/// ignore it.
pub struct ProviderRequest<'a> {
    pub session_id: &'a str,
    pub model: &'a ModelConfig,
    pub messages: &'a [Value],
    pub tools: &'a [ToolDef],
    pub tool_choice: Option<&'a ToolChoice>,
    pub env: &'a HashMap<OsString, OsString>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    type Extras: DeserializeOwned + Default + Clone + Send + Sync + 'static;
    type BuildError: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;
    type Error: std::fmt::Debug + std::fmt::Display + Send + Sync + 'static;

    fn new(
        provider_config: ProviderConfig,
        extras: Self::Extras,
    ) -> Result<Arc<Self>, Self::BuildError>
    where
        Self: Sized;

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_chunk: &mut (dyn for<'v> FnMut(&'v Value) -> Result<(), Self::Error> + Send),
    ) -> Result<(), Self::Error>;

    async fn complete(&self, req: ProviderRequest<'_>) -> Result<CompletionOutcome, Self::Error>;
}

/// Boxed error type used at the [`ErasedProvider`] boundary. Any concrete
/// `Provider::Error` that implements `std::error::Error + Send + Sync +
/// 'static` (or has a `From`/`Into` for this box, like `anyhow::Error`)
/// converts in both directions.
pub type ErasedError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type ErasedResult<T> = std::result::Result<T, ErasedError>;

/// Sealed type-erased view over any [`Provider`]. The cache stores these.
#[derive(Clone)]
pub struct ErasedProvider {
    inner: Arc<dyn ErasedInner>,
}

trait ErasedInner: Send + Sync {
    fn stream<'a>(
        &'a self,
        req: ProviderRequest<'a>,
        on_chunk: &'a mut (dyn for<'v> FnMut(&'v Value) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<()>>;

    fn complete<'a>(
        &'a self,
        req: ProviderRequest<'a>,
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>>;
}

impl<P> ErasedInner for P
where
    P: Provider + 'static,
    P::Error: Into<ErasedError> + From<ErasedError>,
{
    fn stream<'a>(
        &'a self,
        req: ProviderRequest<'a>,
        on_chunk: &'a mut (dyn for<'v> FnMut(&'v Value) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<()>> {
        Box::pin(async move {
            let mut chunk_err: Option<ErasedError> = None;
            let mut wrapped = |v: &Value| -> std::result::Result<(), P::Error> {
                match on_chunk(v) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let synthesised: P::Error = (Box::new(ChunkAbort) as ErasedError).into();
                        chunk_err = Some(e);
                        Err(synthesised)
                    }
                }
            };
            let res = self.stream(req, &mut wrapped).await;
            if let Some(e) = chunk_err {
                return Err(e);
            }
            res.map_err(P::Error::into)
        })
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest<'a>,
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>> {
        Box::pin(async move { self.complete(req).await.map_err(P::Error::into) })
    }
}

#[derive(Debug)]
struct ChunkAbort;
impl std::fmt::Display for ChunkAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("on_chunk callback aborted")
    }
}
impl std::error::Error for ChunkAbort {}

impl ErasedProvider {
    pub fn new<P>(provider: Arc<P>) -> Self
    where
        P: Provider + 'static,
        P::Error: Into<ErasedError> + From<ErasedError>,
    {
        Self { inner: provider }
    }

    pub async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_chunk: &mut (dyn for<'v> FnMut(&'v Value) -> ErasedResult<()> + Send),
    ) -> ErasedResult<()> {
        self.inner.stream(req, on_chunk).await
    }

    pub async fn complete(&self, req: ProviderRequest<'_>) -> ErasedResult<CompletionOutcome> {
        self.inner.complete(req).await
    }
}

pub struct OpenAiLikeProvider {
    provider_config: ProviderConfig,
    extras: ResponsesModelExtras,
    http: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum OpenAiLikeError {
    #[error("build reqwest client: {0}")]
    BuildClient(#[source] reqwest::Error),
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
    #[error("serialize tool definitions: {0}")]
    SerializeTools(#[source] serde_json::Error),
    #[error("serialize tool_choice: {0}")]
    SerializeToolChoice(#[source] serde_json::Error),
    #[error("parse extra_completion_properties as JSON: {0}")]
    ParseExtras(#[source] serde_json::Error),
    #[error("extra_completion_properties must be a JSON object, got {0}")]
    ExtrasNotObject(&'static str),
    #[error("HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    BadStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("read stream chunk: {0}")]
    StreamChunk(#[source] reqwest::Error),
    #[error("on_chunk callback aborted: {0}")]
    OnChunk(ErasedError),
    #[error("tool call accumulator: {0}")]
    Accumulator(ErasedError),
}

impl From<ErasedError> for OpenAiLikeError {
    fn from(e: ErasedError) -> Self {
        Self::OnChunk(e)
    }
}

#[async_trait]
impl Provider for OpenAiLikeProvider {
    type Extras = ResponsesModelExtras;
    type BuildError = OpenAiLikeError;
    type Error = OpenAiLikeError;

    fn new(
        provider_config: ProviderConfig,
        extras: ResponsesModelExtras,
    ) -> std::result::Result<Arc<Self>, OpenAiLikeError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(OpenAiLikeError::BuildClient)?;
        Ok(Arc::new(Self {
            provider_config,
            extras,
            http,
        }))
    }

    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_chunk: &mut (
                 dyn for<'v> FnMut(&'v Value) -> std::result::Result<(), OpenAiLikeError> + Send
             ),
    ) -> std::result::Result<(), OpenAiLikeError> {
        let _ = req.session_id; // OpenAI auto-caches; we don't need to thread the id today.
        let plan = self.build_request_plan(req.model, req.env)?;

        let mut body = serde_json::json!({
            "model": plan.model.id,
            "messages": req.messages,
            "max_tokens": plan.model.max_tokens,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] =
                serde_json::to_value(req.tools).map_err(OpenAiLikeError::SerializeTools)?;
        }
        if let Some(tc) = req.tool_choice {
            body["tool_choice"] =
                serde_json::to_value(tc).map_err(OpenAiLikeError::SerializeToolChoice)?;
        }
        merge_extras(&mut body, plan.extra_completion_properties.as_deref())?;

        debug!(
            messages = req.messages.len(),
            tools = req.tools.len(),
            url = %plan.url,
            model = %plan.model.id,
            "calling chat completions"
        );
        trace!(body = %body, "chat completions request body");

        let mut request = self
            .http
            .post(plan.url)
            .timeout(Duration::from_millis(plan.model.stream_idle_timeout_ms))
            .bearer_auth(&plan.bearer_token)
            .json(&body);
        for (k, v) in &plan.headers {
            request = request.header(k, v);
        }

        let response = request.send().await.map_err(OpenAiLikeError::Http)?;
        trace!(status = %response.status(), headers = ?response.headers(), "chat completions response head");

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenAiLikeError::BadStatus { status, body });
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(OpenAiLikeError::StreamChunk)?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(idx) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..idx + 2).collect();
                for line in frame.lines() {
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload.is_empty() || payload == "[DONE]" {
                        continue;
                    }

                    trace!(payload, "chat completions sse chunk");
                    let value: Value = match serde_json::from_str(payload) {
                        Ok(value) => value,
                        Err(error) => {
                            trace!(%error, payload, "skipping unparsable sse payload");
                            continue;
                        }
                    };

                    on_chunk(&value)?;
                }
            }
        }

        Ok(())
    }

    async fn complete(
        &self,
        req: ProviderRequest<'_>,
    ) -> std::result::Result<CompletionOutcome, OpenAiLikeError> {
        let mut text = String::new();
        let mut accumulator = ToolCallAccumulator::new();
        let mut acc_err: Option<ErasedError> = None;
        <Self as Provider>::stream(self, req, &mut |chunk| {
            for delta in chunk_text_deltas(chunk) {
                text.push_str(delta);
            }
            for delta in chunk_tool_call_deltas(chunk) {
                if let Err(e) = accumulator.push(delta) {
                    acc_err = Some(Box::<dyn std::error::Error + Send + Sync + 'static>::from(
                        e.to_string(),
                    ));
                    return Err(OpenAiLikeError::Accumulator(acc_err.take().unwrap()));
                }
            }
            Ok(())
        })
        .await?;
        let tool_calls = accumulator
            .finalize()
            .map_err(|e| OpenAiLikeError::Accumulator(Box::from(e.to_string())))?;
        Ok(CompletionOutcome { text, tool_calls })
    }
}

impl OpenAiLikeProvider {
    fn build_request_plan(
        &self,
        model: &ModelConfig,
        env: &HashMap<OsString, OsString>,
    ) -> std::result::Result<RequestPlan, OpenAiLikeError> {
        let bearer_token = resolve_bearer(&self.provider_config.auth, env)?;
        let url = self
            .provider_config
            .base_url
            .join("chat/completions")
            .map_err(OpenAiLikeError::JoinBaseUrl)?;
        let headers = expand_headers(&self.provider_config.http_headers, env)?;
        let extra_completion_properties = self.extras.extra_completion_properties.clone();
        Ok(RequestPlan {
            url,
            bearer_token,
            headers,
            extra_completion_properties,
            model: model.clone(),
        })
    }
}

struct RequestPlan {
    url: Url,
    bearer_token: String,
    headers: Vec<(String, String)>,
    model: ModelConfig,
    extra_completion_properties: Option<String>,
}

fn resolve_bearer(
    auth: &AuthMethod,
    env: &HashMap<OsString, OsString>,
) -> std::result::Result<String, OpenAiLikeError> {
    match auth {
        AuthMethod::EnvKey {
            env_key,
            env_key_instructions,
        } => env
            .get(std::ffi::OsStr::new(env_key))
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or_else(|| match env_key_instructions {
                Some(hint) => OpenAiLikeError::MissingEnvVarHinted {
                    var: env_key.clone(),
                    hint: hint.clone(),
                },
                None => OpenAiLikeError::MissingEnvVar(env_key.clone()),
            }),
        AuthMethod::Token { token } => Ok(token.clone()),
        AuthMethod::File { file } => std::fs::read_to_string(file)
            .map(|s| s.trim().to_owned())
            .map_err(|source| OpenAiLikeError::ReadAuthFile {
                path: file.clone(),
                source,
            }),
        AuthMethod::Command { .. } => Err(OpenAiLikeError::AuthCommandUnimplemented),
    }
}

fn expand_headers(
    raw: &BTreeMap<String, frances_config::EnvString>,
    env: &dyn EnvLookup,
) -> std::result::Result<Vec<(String, String)>, OpenAiLikeError> {
    let mut out = Vec::with_capacity(raw.len());
    for (name, template) in raw {
        if name.eq_ignore_ascii_case("authorization") {
            tracing::warn!(
                header = %name,
                "Authorization header in http_headers is ignored — auth resolves it"
            );
            continue;
        }
        let value = template
            .expand(env)
            .map_err(|source| OpenAiLikeError::ExpandHeader {
                name: name.clone(),
                source,
            })?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

fn merge_extras(
    body: &mut Value,
    extras: Option<&str>,
) -> std::result::Result<(), OpenAiLikeError> {
    let Some(extras) = extras else {
        return Ok(());
    };
    let parsed: Value = serde_json::from_str(extras).map_err(OpenAiLikeError::ParseExtras)?;
    let Value::Object(extras_obj) = parsed else {
        return Err(OpenAiLikeError::ExtrasNotObject(type_name_of(&parsed)));
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
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn merge_extras_none_is_noop() {
        let mut body = json!({"a": 1});
        merge_extras(&mut body, None).unwrap();
        assert_eq!(body, json!({"a": 1}));
    }
}
