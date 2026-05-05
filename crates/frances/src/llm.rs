use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, trace};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MODEL: &str = "qwen/qwen3-coder-next";
const PROVIDER_ORDER: &[&str] = &["parasail"];
const MAX_TOKENS: u32 = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct InceptionClient {
    http: reqwest::Client,
    api_key: String,
}

impl InceptionClient {
    pub fn from_env(env: &[(OsString, OsString)]) -> Result<Self> {
        let api_key = env
            .iter()
            .find(|(k, _)| k == "OPENROUTER_API_KEY")
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("OPENROUTER_API_KEY not set in client environment"))?;

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("build reqwest client")?;

        Ok(Self { http, api_key })
    }

    pub async fn stream<F>(&self, messages: &[Value], mut on_chunk: F) -> Result<()>
    where
        F: FnMut(&Value) -> Result<()>,
    {
        let body = ChatRequest {
            model: MODEL,
            messages,
            max_tokens: MAX_TOKENS,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            reasoning: Reasoning { enabled: true },
            provider: Provider {
                order: PROVIDER_ORDER,
            },
        };

        debug!(messages = messages.len(), "calling openrouter chat completions");

        let response = self
            .http
            .post(OPENROUTER_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("openrouter request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("openrouter returned {status}: {text}"));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("inception stream chunk")?;
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
}

pub fn chunk_text_deltas(chunk: &Value) -> impl Iterator<Item = &str> {
    chunk
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
        })
        .filter(|text| !text.is_empty())
}

pub fn chunk_usage(chunk: &Value) -> Option<Usage> {
    let usage = chunk.get("usage")?;
    if usage.is_null() {
        return None;
    }
    Some(Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        cached_input_tokens: usage
            .get("cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_input_tokens: u32,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Value],
    max_tokens: u32,
    stream: bool,
    stream_options: StreamOptions,
    reasoning: Reasoning,
    provider: Provider<'a>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct Reasoning {
    enabled: bool,
}

#[derive(Serialize)]
struct Provider<'a> {
    order: &'a [&'a str],
}
