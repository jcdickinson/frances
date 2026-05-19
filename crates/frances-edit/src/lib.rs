mod anchor;
mod edit;
mod engine;
mod loop_guard;
mod pool;
mod reconcile;
mod render;
mod session;
mod state;
mod store;
mod truncated;

pub use anchor::{Anchor, AnchorParseError};
pub use edit::{EditOp, apply_ops};
pub use engine::{EditEngine, WorkingFile};
pub use loop_guard::LoopKey;
pub use pool::Pool;
pub use reconcile::{EditHints, ReconcileOutcome, reconcile};
pub use render::{render_diff_block, render_file};
pub use session::{EditError, EditResult, EditSession, LlmEdit};
pub use state::{FileAnchorState, LineEntry, content_digest};
pub use store::{AnchorStore, StoreError, StoreResult};
pub use truncated::Truncated;

#[cfg(any(test, feature = "test-utils"))]
pub use session::test_support;
#[cfg(any(test, feature = "test-utils"))]
pub use store::FakeStore;
