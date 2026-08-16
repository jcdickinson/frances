mod client;
mod search;
mod server;

pub use client::{Client, ClientError, WorkerShell};
pub use search::{SearchError, SearchOutcome, find_or_grep};
pub use server::serve;
