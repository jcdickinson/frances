use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::{ModelConfig, ProviderConfig};

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

/// Streaming events emitted by [`Provider::stream`].
///
/// Tool calls are emitted once each as a fully-parsed [`ToolCall`].
/// OpenAI-shaped wires can't reliably mark per-call completion mid-stream,
/// so the implementation fires these at end-of-stream just before the
/// stream returns; future wires that signal per-call completion can emit
/// them as they finish. The same calls are also returned in
/// [`CompletionOutcome::tool_calls`] for callers who'd rather batch.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A fragment of assistant text. Concatenate to obtain the running text.
    TextDelta(String),
    /// A completed tool call.
    ToolCall(ToolCall),
    /// Final-frame token accounting. May be emitted once at the end of the
    /// stream; not all wires populate it.
    Usage(Usage),
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

    /// Drive a single chat call. Typed [`StreamEvent`]s are delivered to
    /// `on_event` synchronously as they're parsed; the call's full result
    /// (concatenated text + finalised tool calls) is returned at the end.
    async fn stream(
        &self,
        req: ProviderRequest<'_>,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), Self::Error> + Send),
    ) -> Result<CompletionOutcome, Self::Error>;

    /// Convenience: same as [`stream`](Self::stream) with a no-op event
    /// handler. Override only if you can be cheaper than the default.
    async fn complete(
        &self,
        req: ProviderRequest<'_>,
    ) -> Result<CompletionOutcome, Self::Error> {
        self.stream(req, &mut |_| Ok(())).await
    }
}

/// Boxed error type used at the [`ErasedProvider`] boundary. Any concrete
/// `Provider::Error` that converts in both directions with this box (e.g. a
/// thiserror enum that derives `Error`, plus a manual `From<ErasedError>`)
/// can be wrapped.
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
        on_event: &'a mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>>;

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
        on_event: &'a mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> BoxFuture<'a, ErasedResult<CompletionOutcome>> {
        Box::pin(async move {
            let mut event_err: Option<ErasedError> = None;
            let mut wrapped = |ev: StreamEvent| -> std::result::Result<(), P::Error> {
                match on_event(ev) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let synthesised: P::Error = (Box::new(ChunkAbort) as ErasedError).into();
                        event_err = Some(e);
                        Err(synthesised)
                    }
                }
            };
            let res = self.stream(req, &mut wrapped).await;
            if let Some(e) = event_err {
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
pub struct ChunkAbort;
impl std::fmt::Display for ChunkAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("on_event callback aborted")
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
        on_event: &mut (dyn FnMut(StreamEvent) -> ErasedResult<()> + Send),
    ) -> ErasedResult<CompletionOutcome> {
        self.inner.stream(req, on_event).await
    }

    pub async fn complete(&self, req: ProviderRequest<'_>) -> ErasedResult<CompletionOutcome> {
        self.inner.complete(req).await
    }
}

/// Final result of a [`Provider::stream`] / [`Provider::complete`] call.
/// `text` is the concatenation of all `TextDelta` events; `tool_calls` is
/// the parsed tool-call list (ordered by index).
#[derive(Debug, Clone)]
pub struct CompletionOutcome {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Wire shape: `{"type": "function", "function": {...}}`. Adjacently tagged
/// so the variant name becomes the `type` value and the inner struct sits
/// under the `function` key. New tool kinds (if OpenAI ever ships them)
/// would be added as additional variants here.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", content = "function", rename_all = "snake_case")]
pub enum ToolDef {
    Function(ToolFunction),
}

#[derive(Serialize, Clone, Debug)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Per OpenAI's spec, `tool_choice` is either a string mode (`"auto"`,
/// `"none"`, `"required"`) or an object pinning a specific function:
/// `{"type":"function","function":{"name":"..."}}`. This enum serializes to
/// whichever shape is appropriate. Variants are kept for caller flexibility;
/// callers default to `auto` by omitting `tool_choice` from the request.
#[derive(Clone, Debug)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

impl Serialize for ToolChoice {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Auto => ser.serialize_str("auto"),
            Self::None => ser.serialize_str("none"),
            Self::Required => ser.serialize_str("required"),
            Self::Function(name) => {
                let mut map = ser.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;
                map.serialize_entry("function", &serde_json::json!({ "name": name }))?;
                map.end()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Token-usage report. Universal shape; `cached_input_tokens` mirrors
/// OpenAI's `prompt_tokens_details.cached_tokens` for the wires that
/// surface it.
#[derive(Debug, Clone, Default, serde::Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tooldef_serializes_to_openai_shape() {
        let td = ToolDef::Function(ToolFunction {
            name: "edit".into(),
            description: "Apply a patch".into(),
            parameters: json!({"type": "object"}),
        });
        let serialized = serde_json::to_value(&td).unwrap();
        assert_eq!(serialized["type"], "function");
        assert_eq!(serialized["function"]["name"], "edit");
        assert_eq!(serialized["function"]["description"], "Apply a patch");
    }

    #[test]
    fn toolchoice_modes_serialize_to_strings() {
        assert_eq!(serde_json::to_value(&ToolChoice::Auto).unwrap(), "auto");
        assert_eq!(serde_json::to_value(&ToolChoice::None).unwrap(), "none");
        assert_eq!(
            serde_json::to_value(&ToolChoice::Required).unwrap(),
            "required"
        );
    }

    #[test]
    fn toolchoice_function_serializes_to_object() {
        let v = serde_json::to_value(ToolChoice::Function("edit".into())).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "edit");
    }
}
