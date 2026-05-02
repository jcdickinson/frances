use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::history::{BlockType, Message, Role};

const INCEPTION_URL: &str = "https://api.inceptionlabs.ai/v1/chat/completions";
const INCEPTION_MODEL: &str = "mercury-2";
const INCEPTION_MAX_TOKENS: u32 = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct InceptionClient {
    http: reqwest::Client,
    api_key: String,
    session_affinity: String,
}

impl InceptionClient {
    pub fn from_env(env: &[(OsString, OsString)], session_affinity: String) -> Result<Self> {
        let api_key = env
            .iter()
            .find(|(k, _)| k == "INCEPTION_API_KEY")
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("INCEPTION_API_KEY not set in client environment"))?;

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("build reqwest client")?;

        Ok(Self {
            http,
            api_key,
            session_affinity,
        })
    }

    pub async fn stream<F>(&self, messages: &[Message], mut on_event: F) -> Result<()>
    where
        F: FnMut(StreamEvent<'_>) -> Result<()>,
    {
        let body = ChatRequest {
            model: INCEPTION_MODEL,
            messages: messages.iter().filter_map(to_chat_message).collect(),
            max_tokens: INCEPTION_MAX_TOKENS,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        debug!(
            messages = body.messages.len(),
            "calling inception chat completions"
        );

        let response = self
            .http
            .post(INCEPTION_URL)
            .bearer_auth(&self.api_key)
            .header("x-session-affinity", &self.session_affinity)
            .json(&body)
            .send()
            .await
            .context("inception request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("inception returned {status}: {text}"));
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("inception stream chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(idx) = buffer.find("\n\n") {
                let event: String = buffer.drain(..idx + 2).collect();
                for line in event.lines() {
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload.is_empty() || payload == "[DONE]" {
                        continue;
                    }

                    let parsed: ChatStreamEvent = match serde_json::from_str(payload) {
                        Ok(value) => value,
                        Err(error) => {
                            trace!(%error, payload, "skipping unparsable sse payload");
                            continue;
                        }
                    };

                    for choice in parsed.choices {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                on_event(StreamEvent::Text(&content))?;
                            }
                        }
                    }

                    if let Some(usage) = parsed.usage {
                        on_event(StreamEvent::Usage(usage))?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent<'a> {
    Text(&'a str),
    Usage(Usage),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

fn to_chat_message(message: &Message) -> Option<ChatMessage> {
    let role = match message.role {
        Role::User | Role::System | Role::Tool => "user",
        Role::Assistant => "assistant",
    };

    let content = message
        .blocks
        .iter()
        .filter_map(|block| match block.kind {
            BlockType::Text => Some(block.text.as_str()),
            BlockType::Thinking | BlockType::Image | BlockType::ToolUse | BlockType::ToolResult => {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if content.is_empty() {
        return None;
    }

    Some(ChatMessage { role, content })
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatStreamEvent {
    #[serde(default)]
    choices: Vec<ChatStreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ChatStreamChoice {
    #[serde(default)]
    delta: ChatStreamDelta,
}

#[derive(Default, Deserialize)]
struct ChatStreamDelta {
    #[serde(default)]
    content: Option<String>,
}
