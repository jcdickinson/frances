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

pub use anchor::{Anchor, AnchorParseError};
pub use edit::{EditOp, apply_ops};
pub use engine::{EditEngine, WorkingFile};
pub use loop_guard::LoopKey;
pub use pool::Pool;
pub use reconcile::{EditHints, ReconcileOutcome, reconcile};
pub use render::{DiffOp, DiffRender, render_diff_block, render_file};
pub use session::{EditError, EditResult, EditSession, LlmEdit, WriteMode};
pub use state::{FileAnchorState, LineEntry, content_digest};
pub use store::{AnchorStore, StoreError, StoreResult};

/// Content-mismatch errors keep the leading 80 chars of the offending content,
/// appending an ellipsis. A `Cow<'static, str>` so it can live in `EditError`.
pub type Truncated = frances_core::Truncated<'static, 80, true>;

#[cfg(any(test, feature = "test-utils"))]
pub use session::test_support;
#[cfg(any(test, feature = "test-utils"))]
pub use store::FakeStore;
