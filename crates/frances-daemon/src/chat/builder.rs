/// What a caller hands to [`crate::chat::ChatSessionManager::create`] to
/// describe a new chat session.
///
/// Today the only knob is `model_intents` — an ordered list of
/// `models::<intent>` config keys the session walks when picking a model
/// for each call. Construct directly via field-init or chain through
/// [`Self::with_model_intents`].
#[derive(Debug, Clone, Default)]
pub struct ChatSessionBuilder {
    pub model_intents: Vec<String>,
}

impl ChatSessionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model_intents(mut self, intents: impl IntoIterator<Item = String>) -> Self {
        self.model_intents = intents.into_iter().collect();
        self
    }
}
