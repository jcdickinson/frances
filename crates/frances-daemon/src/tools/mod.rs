//! LLM-callable tools.
//!
//! Each [`Tool`] implementation can expose one or more tool definitions
//! (e.g. [`file::FileTools`] exposes `read_file` and the whole `edit_*`
//! family). The [`ToolRegistry`] aggregates a fixed set of tools and
//! routes incoming [`ToolCall`]s to the right implementation.
//!
//! All trait methods are async — including [`Tool::definitions`] — so that
//! a future MCP-backed `Tool` can fetch its schema over HTTP without
//! special-casing.
//!
//! Per-tool descriptions live in `desc/*.md` and are pulled in via
//! `include_str!`. The anchor protocol is taught in the three line-editing
//! tool descriptions (`edit_replace`, `edit_insert_after`,
//! `edit_insert_before`); runtime outputs stay pure data per
//! `docs/arch/anchors.md`.

pub mod file;
pub mod shell;

use std::collections::HashMap;
use std::fs;
use std::num::TryFromIntError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, SystemTimeError};

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use frances_shell::Shell;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::Result;
use crate::anchor_store::AnchorStoreImpl;
use crate::edit_session::EditSession;
use crate::llm::{ToolCall, ToolDef};

pub use file::FileTools;
pub use file::SCHEMA as FILE_SCHEMA;
pub use shell::ShellTools;

#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error("duplicate tool name: {0}")]
    DuplicateTool(String),
    #[error("file modified-time read: {0}")]
    ReadMtime(#[from] std::io::Error),
    #[error("file modified time before unix epoch: {0}")]
    MtimeBeforeEpoch(#[from] SystemTimeError),
    #[error("file mtime ns overflow: {0}")]
    MtimeOverflow(#[from] TryFromIntError),
}

pub struct ToolContext<'a> {
    pub edit_session: &'a Mutex<EditSession<AnchorStoreImpl>>,
    pub shell: &'a Mutex<Option<Shell>>,
    pub cwd: Option<&'a Path>,
}

pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(content: String) -> Self {
        Self {
            content,
            is_error: false,
        }
    }

    fn err(content: String) -> Self {
        Self {
            content,
            is_error: true,
        }
    }

    fn from_result(result: Result<String>) -> Self {
        match result {
            Ok(content) => Self::ok(content),
            Err(error) => Self::err(format!("{error}")),
        }
    }
}

/// One unit of LLM-facing tool functionality. May expose multiple
/// [`ToolDef`]s (the edit family is the canonical example) and routes its
/// own dispatch internally.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool definitions this implementation exposes. Async so that
    /// remote-backed tools (HTTP MCP, etc) can fetch lazily. Called by
    /// [`ToolRegistry`] when warming or refreshing its cache; the registry
    /// holds onto the result, so impls don't need their own cache.
    async fn definitions(&self) -> Result<Vec<ToolDef>>;

    /// Run the call. The registry only routes names that came from this
    /// tool's [`Self::definitions`].
    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext<'_>) -> ToolOutcome;
}

/// Aggregates a fixed set of [`Tool`] implementations and routes calls.
///
/// The combined defs and the name → tool routing map are cached on first
/// use. Use [`Self::refresh`] (e.g. via a future `/tools refresh` slash
/// command) to drop the cache and re-fetch from each tool.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    /// Lock-free reads via [`ArcSwapOption`] — hot path is `dispatch`,
    /// which only needs to look up the routing map. `None` until first
    /// build or after a refresh.
    cache: ArcSwapOption<RegistryCache>,
    /// Serializes builds so concurrent cold-start callers don't all
    /// re-fetch from each tool. Held only across a build, never a read.
    build_lock: Mutex<()>,
}

struct RegistryCache {
    defs: Vec<ToolDef>,
    /// Tool name → index into [`ToolRegistry::tools`].
    name_to_tool: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools,
            cache: ArcSwapOption::const_empty(),
            build_lock: Mutex::new(()),
        }
    }

    /// The built-in registry: file tools (read + edit family) and the
    /// shell trio.
    pub fn builtin() -> Self {
        Self::new(vec![Box::new(FileTools), Box::new(ShellTools)])
    }

    /// Combined definitions from every registered tool, in order. Cached
    /// after the first call; use [`Self::refresh`] to invalidate.
    pub async fn definitions(&self) -> Result<Vec<ToolDef>> {
        if let Some(cache) = self.cache.load_full() {
            return Ok(cache.defs.clone());
        }
        let _guard = self.build_lock.lock().await;
        // Another caller may have built the cache while we waited.
        if let Some(cache) = self.cache.load_full() {
            return Ok(cache.defs.clone());
        }
        let built = self.collect().await?;
        let defs = built.defs.clone();
        self.cache.store(Some(Arc::new(built)));
        Ok(defs)
    }

    /// Drop the cached defs and re-fetch from every tool. Always
    /// re-fetches, even if a concurrent caller just rebuilt — that's the
    /// point of an explicit refresh.
    pub async fn refresh(&self) -> Result<Vec<ToolDef>> {
        let _guard = self.build_lock.lock().await;
        self.cache.store(None);
        let built = self.collect().await?;
        let defs = built.defs.clone();
        self.cache.store(Some(Arc::new(built)));
        Ok(defs)
    }

    pub async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext<'_>) -> ToolOutcome {
        let cache = match self.ensure_cache().await {
            Ok(cache) => cache,
            Err(error) => {
                return ToolOutcome::err(format!("tool registry init failed: {error}"));
            }
        };
        let Some(&idx) = cache.name_to_tool.get(&call.name) else {
            return ToolOutcome::err(format!("unknown tool: {}", call.name));
        };
        self.tools[idx].dispatch(call, ctx).await
    }

    async fn ensure_cache(&self) -> Result<Arc<RegistryCache>> {
        if let Some(cache) = self.cache.load_full() {
            return Ok(cache);
        }
        self.definitions().await?;
        Ok(self
            .cache
            .load_full()
            .expect("tool registry cache populated by definitions() above; ArcSwap snapshot stale"))
    }

    async fn collect(&self) -> Result<RegistryCache> {
        let mut defs = Vec::new();
        let mut name_to_tool = HashMap::new();
        for (idx, tool) in self.tools.iter().enumerate() {
            for def in tool.definitions().await? {
                let ToolDef::Function(function) = &def;
                if name_to_tool.insert(function.name.clone(), idx).is_some() {
                    return Err(ToolRegistryError::DuplicateTool(function.name.clone()).into());
                }
                defs.push(def);
            }
        }
        Ok(RegistryCache { defs, name_to_tool })
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

fn resolve_path(cwd: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn split_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.split('\n').map(str::to_owned).collect();
    // `"a\nb\n".split('\n')` yields `["a", "b", ""]`. The trailing empty
    // element represents the final newline, not an actual blank line on
    // disk. Strip it so the line count matches what's visible. A genuinely
    // blank final line (file ending in `\n\n`) yields `["a", "b", "", ""]`
    // — we strip only the last one, preserving the real blank.
    if lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines
}

fn mtime_ns_from(meta: &fs::Metadata) -> std::result::Result<i64, ToolRegistryError> {
    let modified = meta.modified()?;
    let dur = modified.duration_since(SystemTime::UNIX_EPOCH)?;
    Ok(i64::try_from(dur.as_nanos())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_strips_trailing_newline_only() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb\n\n"), vec!["a", "b", ""]);
        assert_eq!(split_lines(""), Vec::<String>::new());
    }
}
