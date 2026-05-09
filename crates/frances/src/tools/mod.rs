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

mod file;
mod shell;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use frances_shell::Shell;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::anchor_store::AnchorStoreImpl;
use crate::edit_session::EditSession;
use crate::llm::{ToolCall, ToolDef};

pub use file::FileTools;
pub use shell::ShellTools;

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
            Err(error) => Self::err(format!("{error:#}")),
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
    #[expect(dead_code, reason = "wired up later for /tools refresh slash command")]
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
                return ToolOutcome::err(format!("tool registry init failed: {error:#}"));
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
        self.cache
            .load_full()
            .ok_or_else(|| anyhow!("tool registry cache empty after build"))
    }

    async fn collect(&self) -> Result<RegistryCache> {
        let mut defs = Vec::new();
        let mut name_to_tool = HashMap::new();
        for (idx, tool) in self.tools.iter().enumerate() {
            for def in tool.definitions().await? {
                let ToolDef::Function(function) = &def;
                if name_to_tool.insert(function.name.clone(), idx).is_some() {
                    return Err(anyhow!("duplicate tool name: {}", function.name));
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

pub fn assistant_payload(text: &str, tool_calls: &[ToolCall]) -> Value {
    let content = if text.is_empty() {
        Value::Null
    } else {
        Value::String(text.to_string())
    };
    let mut payload = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                    }
                })
            })
            .collect();
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("tool_calls".to_string(), Value::Array(calls));
    }
    payload
}

pub fn tool_result_payload(tool_use_id: &str, content: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_use_id,
        "content": content,
    })
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

fn mtime_ns_from(meta: &fs::Metadata) -> Result<i64> {
    let modified = meta.modified().context("modified time")?;
    let dur = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("modified before unix epoch")?;
    i64::try_from(dur.as_nanos()).context("mtime overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    #[test]
    fn assistant_payload_with_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({ "path": "x.rs" }),
        }];
        let payload = assistant_payload("hello", &calls);
        assert_eq!(payload["role"], "assistant");
        assert_eq!(payload["content"], "hello");
        let tcs = payload["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["type"], "function");
        assert_eq!(tcs[0]["function"]["name"], "read_file");
        let args_str = tcs[0]["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed, json!({ "path": "x.rs" }));
    }

    #[test]
    fn assistant_payload_empty_text_yields_null_content() {
        let payload = assistant_payload("", &[]);
        assert_eq!(payload["role"], "assistant");
        assert!(payload["content"].is_null());
        assert!(payload.get("tool_calls").is_none());
    }

    #[test]
    fn tool_result_payload_uses_openai_field_name() {
        let payload = tool_result_payload("call_1", "anchored content");
        assert_eq!(payload["role"], "tool");
        assert_eq!(payload["tool_call_id"], "call_1");
        assert_eq!(payload["content"], "anchored content");
    }

    #[test]
    fn split_lines_strips_trailing_newline_only() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb\n\n"), vec!["a", "b", ""]);
        assert_eq!(split_lines(""), Vec::<String>::new());
    }
}
