use crate::chat::store::HistoryStore;

/// Implementation-side deps that the concrete `ChatSessionManager` reads
/// from. Workflow doesn't see this trait; only `frances-llm` and the
/// session-runtime impl care.
pub trait ChatManagerDeps: Clone + Send + Sync + 'static {
    type HistoryStore: HistoryStore + Clone;

    fn history_store(&self) -> &Self::HistoryStore;
}
