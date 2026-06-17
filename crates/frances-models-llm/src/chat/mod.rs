pub mod builder;
pub mod complete;
pub mod error;
pub mod session;
pub mod types;

pub use crate::history::{BatchRow, HistoryBatch, OwnedHistoryInput};
pub use builder::{ChatSessionBuilder, ModelIntents};
pub use complete::{CompleteRequest, Demand, EnforceError};
pub use error::{ChatError, HistoryError};
pub use session::{ChatSession, ChatSessionManager};
pub use types::{ChatSessionId, ChatSessionRow};
