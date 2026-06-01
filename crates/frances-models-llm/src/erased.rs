//! Type-erased error plumbing for the provider boundary. Concrete provider
//! errors box into [`ErasedError`]; [`ChunkAbort`] is the sentinel boxed in
//! when a caller-provided `on_event` callback aborts a stream.

/// Boxed error type used at the type-erased provider boundary. Any concrete
/// provider error that converts in both directions with this box (e.g. a
/// thiserror enum that derives `Error`, plus a manual `From<ErasedError>`)
/// can be wrapped.
pub type ErasedError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type ErasedResult<T> = std::result::Result<T, ErasedError>;

/// Signal value used at the type-erased provider boundary to abort a
/// stream when the caller-provided `on_event` callback returned an error.
#[derive(Debug)]
pub struct ChunkAbort;

impl std::fmt::Display for ChunkAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("on_event callback aborted")
    }
}

impl std::error::Error for ChunkAbort {}
