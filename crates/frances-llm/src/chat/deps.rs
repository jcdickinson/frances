use crate::chat::store::HistoryStore;

/// Implementation-side deps that the concrete `ChatSessionManager` reads
/// from. Workflow doesn't see this trait; only `frances-llm` and the
/// session-runtime impl care.
///
/// Clone-by-value: each impl wraps its complex state in `Arc<Inner>`
/// internally so cloning the outer handle is cheap.
pub trait ChatManagerDeps: Clone + Send + Sync + 'static {
    type HistoryStore: HistoryStore + Clone;

    fn history_store(&self) -> &Self::HistoryStore;
}
