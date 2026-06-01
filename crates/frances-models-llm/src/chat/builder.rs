use std::borrow::Cow;

/// Ordered list of `models::<intent>` config keys the session walks
/// when picking a model.
pub type ModelIntents = Cow<'static, [Cow<'static, str>]>;

/// What a caller hands to `ChatSessionManager::create` to describe a new
/// chat session.
///
/// - `model_intents` — ordered list of `models::<intent>` config keys
///   the session walks when picking a model for each call.
/// - `ephemeral` — when `true`, the session never reads or writes the
///   `chat_sessions` / `chat_messages` tables. The provider sees only
///   what JS has pushed since the last `stream()`. Default `false`.
#[derive(Debug, Clone, Default)]
pub struct ChatSessionBuilder {
    pub model_intents: ModelIntents,
    pub ephemeral: bool,
}

impl ChatSessionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model_intents(mut self, intents: impl Into<ModelIntents>) -> Self {
        self.model_intents = intents.into();
        self
    }

    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }
}
