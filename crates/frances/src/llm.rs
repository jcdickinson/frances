use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use llm::backends::openai::OpenAI;
use llm::chat::{ChatMessage, ChatProvider, ChatRole, StreamChunk};
use tracing::{debug, trace};

use crate::history::{Block, BlockType, Message, Role};

pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4.1-nano";

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Thinking(String),
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
    pub system_prompt: Option<String>,
}

impl OpenAiConfig {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is not set")?;
        let base_url = std::env::var("OPENAI_BASE_URL").ok();
        let model =
            std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
        let system_prompt = std::env::var("FRANCES_SYSTEM_PROMPT").ok();

        Ok(Self {
            api_key,
            base_url,
            model,
            system_prompt,
        })
    }
}

pub struct OpenAiProvider {
    client: OpenAI,
    system_prompt: Option<String>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("system_prompt", &self.system_prompt)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self> {
        debug!(model = %config.model, has_base_url = config.base_url.is_some(), "constructing openai provider");

        let client = OpenAI::new(
            config.api_key,
            config.base_url,
            Some(config.model),
            None,
            None,
            None,
            config.system_prompt.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .map_err(|error| anyhow!(error.to_string()))?;

        Ok(Self {
            client,
            system_prompt: config.system_prompt,
        })
    }

    pub fn prepend_system<'a>(
        &self,
        messages: impl IntoIterator<Item = &'a Message>,
    ) -> Vec<ChatMessage> {
        let mut out = Vec::new();

        if let Some(prompt) = &self.system_prompt {
            out.push(ChatMessage::assistant().content(prompt.clone()).build());
        }

        out.extend(messages.into_iter().map(to_chat_message));
        out
    }

    pub async fn stream<'a>(
        &self,
        messages: impl IntoIterator<Item = &'a Message>,
    ) -> Result<Vec<StreamEvent>> {
        let request = self.prepend_system(messages);
        debug!(messages = request.len(), "starting llm stream");

        let mut stream = self
            .client
            .chat_stream_with_tools(&request, None)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;

        let mut events = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk.map_err(|error| anyhow!(error.to_string()))? {
                StreamChunk::Text(text) => {
                    trace!(bytes = text.len(), "received llm text chunk");
                    events.push(StreamEvent::Text(text));
                }
                StreamChunk::Done { stop_reason } => {
                    trace!(%stop_reason, "llm stream completed");
                }
                StreamChunk::ToolUseStart { name, .. } => {
                    trace!(tool = %name, "ignoring llm tool call start");
                }
                StreamChunk::ToolUseInputDelta { .. } | StreamChunk::ToolUseComplete { .. } => {
                    trace!("ignoring llm tool call chunk");
                }
            }
        }

        Ok(events)
    }
}

fn to_chat_message(message: &Message) -> ChatMessage {
    let role = match message.role {
        Role::System | Role::User | Role::Tool => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
    };

    let content = message
        .blocks
        .iter()
        .filter_map(|block| match block.kind {
            BlockType::Text | BlockType::Thinking => Some(block.text.as_str()),
            BlockType::Image | BlockType::ToolUse | BlockType::ToolResult => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    ChatMessage::new(role).content(content).build()
}

trait ChatMessageExt {
    fn new(role: ChatRole) -> llm::chat::ChatMessageBuilder;
}

impl ChatMessageExt for ChatMessage {
    fn new(role: ChatRole) -> llm::chat::ChatMessageBuilder {
        match role {
            ChatRole::User => ChatMessage::user(),
            ChatRole::Assistant => ChatMessage::assistant(),
        }
    }
}

#[allow(dead_code)]
fn _block_to_text(block: &Block) -> Option<&str> {
    match block.kind {
        BlockType::Text | BlockType::Thinking => Some(block.text.as_str()),
        BlockType::Image | BlockType::ToolUse | BlockType::ToolResult => None,
    }
}
