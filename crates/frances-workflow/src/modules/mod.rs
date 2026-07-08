//! Virtual modules exposed to workflow scripts.
//!
//! Two families:
//!
//! - `frances:v1/*` — the workflow host API. Per-invocation values
//!   (channels, flags) flow through a transient stash on `globalThis`;
//!   each module's body captures them into its local scope and the
//!   stash is then deleted, so user scripts can't reach it.
//! - `whatwg:*` — vendored polyfills under `assets/whatwg/`. They
//!   used to be pure JS with no per-invocation state; today
//!   `whatwg:abortcontroller` also captures `_setSleep` from the
//!   stash, so the stash must be live when whatwg modules evaluate.
//!
//! Install flow (orchestrated by the runtime):
//!
//! 1. `install_stash` — build per-invocation host values, place them on
//!    `globalThis.__frances_v1_stash__`.
//! 2. `install_whatwg` — declare + eval `whatwg:*`. The polyfills that
//!    need stash entries grab them into their module scope here.
//! 3. `install_v1_modules` — declare + eval `frances:v1/*`. Each module
//!    body destructures `globalThis.__frances_v1_stash__` to capture the
//!    slots it needs into module scope.
//! 4. `remove_stash` — delete the stash from `globalThis` so user
//!    scripts can't reach it.
//!
//! Module source files and prompt markdown live under `assets/`, embedded
//! as one VFS via `rust-embed`.
//!
//! Modules:
//!
//! - `frances:v1/workflow`       — `exit` lifecycle function.
//! - `frances:v1/inbox`          — `inbox` async-iterable user-input stream.
//! - `frances:v1/sections`         — `transcript`, `MarkdownSection`, `ErrorSection`,
//!   `JsonSection` (frame-objects-with-history API).
//! - `frances:v1/chat`           — `ChatSession` (LLM access).
//! - `frances:v1/io`             — `Timer` + `TimerError`. The user-facing
//!   surface is pure JS in `assets/frances/v1/io.js`; Rust exposes a private sleep
//!   primitive (`_setSleep` / `_clearSleep`) on the install-time stash
//!   that the JS wrapper composes against.
//! - `frances:v1/approval`       — single async
//!   `approve({ prompt, toolCall?, allowAuto? })` that asks the user
//!   for permission. Backed by a private `_approve` primitive on the
//!   install stash; the host bridges via the permissions channel, whose
//!   `PermissionRequest` carries its own reply slot.
//! - `frances:v1/storage`        — `db` singleton with
//!   `exec`/`query`/`queryStream`/`transaction`. Backed by a workflow's
//!   per-entity migrations declared in `[workflows.<id>].migrations`.
//! - `frances:v1/tools/shell`    — `Shell` primitive + `Run`/`Wait`/`Kill`
//!   tool classes.
//! - `frances:v1/tools/file`     — `Editor` primitive + `Read`/`ReplaceLines`/
//!   `ReplaceAll`/`InsertAfter`/`InsertBefore`/`New`/`Overwrite` tool classes.
//! - `frances:v1/tools/file_find_or_grep` — `FileSearch` primitive + `Search`
//!   tool class. Combined name-pattern lookup, content search, and
//!   directory listing via the ripgrep crates (`ignore::WalkParallel`
//!   plus `grep-searcher`).
//! - `frances:v1/tools/variable` — pure-JS `Variables` JSON store +
//!   `Get`/`Set` tool classes.
//! - `frances:v1/tool-family`   — pure-JS `defineToolFamily` + `defineTool`
//!   + `toolGuidance` section. Identity-based family dedup, tool construction,
//!     and the prompt section that renders family guidance.
//! - `frances:v1/context-sections` — pure-JS `envBlock` + `cwdBlock` prompt
//!   sections for environment info and working directory.
//! - `frances:v1/agents` — Rust-backed `discoverGlobalAgents`,
//!   `discoverLocalAgents`, `discoverNestedAgents` for instruction
//!   file discovery (AGENTS.md / CLAUDE.md).
//! - `frances:v1/agent-sections` — `globalAgents`, `localAgents`,
//!   `nestedAgentsInventory` prompt sections backed by the agents module.
//! - `whatwg:web-streams`        — `ReadableStream`, `WritableStream`,
//!   `TransformStream` and friends from web-streams-polyfill.
//! - `whatwg:abortcontroller`    — `AbortController`, `AbortSignal`
//!   (hand-rolled, EventTarget-free). `AbortSignal.timeout` builds on
//!   the stash's sleep primitive.
//! - `whatwg:dom`                — minimal DOM Standard surface
//!   (currently just `DOMException`).
//! - `vendor:mustache`           — vendored Mustache template renderer.

use std::sync::Arc;

use rquickjs::loader::{Loader, Resolver};
use rquickjs::module::{Declared, Module};
use rquickjs::{CatchResultExt, Ctx, Error as JsError, Object, Value};
use rust_embed::Embed;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::WorkflowError;
use crate::closed::WorkflowClosed;
use crate::deps::WorkflowDeps;
use crate::runtime::{InboxItem, OutputSenders, caught};

pub mod agents;
pub mod chat;
pub mod file;
pub mod file_find_or_grep;
pub mod inbox;
pub mod io;
pub mod jaq;
pub mod lifecycle;
pub mod permission;
pub mod sections;
pub mod shell;
pub mod storage;
pub mod workflow;

/// Global key on `globalThis` where the module stash lives during
/// install. Deleted after every virtual module has captured its slot,
/// so user scripts can't reach it via `globalThis`.
const STASH_KEY: &str = "__frances_v1_stash__";

#[derive(Embed)]
#[folder = "assets/"]
struct WorkflowAssets;

pub(crate) struct EmbeddedResolver;

impl Resolver for EmbeddedResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
    ) -> rquickjs::Result<String> {
        let resolved = resolve_module_name(base, name);
        if asset_path(&resolved)
            .as_deref()
            .is_some_and(|path| WorkflowAssets::get(path).is_some())
        {
            return Ok(resolved);
        }
        Err(JsError::new_resolving(base, name))
    }
}

pub(crate) struct EmbeddedLoader;

impl Loader for EmbeddedLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, name: &str) -> rquickjs::Result<Module<'js, Declared>> {
        let Some(source) = load_asset_text(name) else {
            return Err(JsError::new_loading(name));
        };
        if name.ends_with(".md") {
            let js = serde_json::to_string(&source)
                .map(|text| format!("export default {text};\n"))
                .map_err(|err| JsError::new_loading_message(name, err.to_string()))?;
            return Module::declare(ctx.clone(), name, js.as_bytes());
        }
        Module::declare(ctx.clone(), name, source.as_bytes())
    }
}

fn resolve_module_name(base: &str, name: &str) -> String {
    if !name.starts_with('.') {
        return name.to_owned();
    }

    let prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or("");
    let mut parts: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn asset_path(module_name: &str) -> Option<String> {
    let path = if let Some(path) = module_name.strip_prefix("frances:v1/") {
        format!("frances/v1/{path}")
    } else if let Some(path) = module_name.strip_prefix("whatwg:") {
        format!("whatwg/{path}")
    } else if let Some(path) = module_name.strip_prefix("vendor:") {
        format!("vendor/{path}")
    } else {
        return None;
    };

    if path.ends_with(".md") || path.ends_with(".js") {
        Some(path)
    } else {
        Some(format!("{path}.js"))
    }
}

fn load_asset_text(module_name: &str) -> Option<String> {
    let path = asset_path(module_name)?;
    let file = WorkflowAssets::get(&path)?;
    std::str::from_utf8(file.data.as_ref())
        .ok()
        .map(str::to_owned)
}

/// Per-invocation host state that gets stashed for module bodies to capture.
pub(crate) struct V1HostState<D: WorkflowDeps> {
    pub senders: OutputSenders,
    pub input_rx: Arc<AsyncMutex<UnboundedReceiver<InboxItem>>>,
    pub closed: Arc<WorkflowClosed>,
    /// Pulsed by `inbox.next()` when it suspends on an empty queue (the
    /// body is idle, waiting for input). Test-harness signal; production
    /// completion is driven by the event loop draining, not this — so it's
    /// compiled only under test.
    #[cfg(any(test, feature = "test-utils"))]
    pub on_idle: Arc<Notify>,
    /// Pulsed when the host (or `exit()`) requests graceful shutdown.
    /// The runtime races this against the event loop; on fire it runs the
    /// workflow's registered shutdown handler (if any) then closes the
    /// inbox.
    pub shutdown_notify: Arc<Notify>,
    pub deps: D,
    pub workflow_db: Arc<crate::storage::WorkflowDb>,
}

/// Builds the install-time stash and places it at
/// `globalThis.__frances_v1_stash__`. Must run before any virtual
/// module is evaluated; the matching `remove_stash` must run once
/// every module has captured its slots.
pub(crate) fn install_stash<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    host: V1HostState<D>,
) -> Result<Object<'js>, WorkflowError> {
    // Bind everything except `on_idle` (which is cfg-gated, and Rust
    // doesn't allow `#[cfg]` on a destructure-pattern field) via `..`,
    // then pull `on_idle` out by field access under the same cfg.
    let V1HostState {
        senders,
        input_rx,
        closed,
        shutdown_notify,
        deps,
        workflow_db,
        ..
    } = host;
    #[cfg(any(test, feature = "test-utils"))]
    let on_idle = host.on_idle;

    // Clone for the approval primitive — it owns a sender into the
    // permissions channel so JS `approve()` can emit a request without
    // going through `transcript`.
    let approval_permissions_tx = senders.permissions.clone();

    let stash = Object::new(ctx.clone())?;

    let exit_fn = workflow::build_exit(ctx, shutdown_notify.clone())?;
    stash.set("exit", exit_fn)?;

    let set_status_fn = workflow::build_set_status(ctx, senders.surfaces.clone())?;
    stash.set("setStatus", set_status_fn)?;

    // The lifecycle hook is invoked by the runtime on shutdown (it reads
    // `lifecycle.shutdown` off the returned object) and the runtime closes
    // the inbox itself, so the module just needs the object exported.
    let lifecycle_obj = lifecycle::build_lifecycle_object(ctx)?;
    stash.set("lifecycle", lifecycle_obj.clone())?;

    let inbox_instance = inbox::build_inbox(
        ctx,
        inbox::InboxArgs {
            rx: input_rx,
            closed: closed.clone(),
            #[cfg(any(test, feature = "test-utils"))]
            on_idle,
        },
    )?;
    stash.set("inbox", inbox_instance)?;

    let (
        transcript_proxy,
        md_ctor,
        err_ctor,
        json_ctor,
        shell_output_ctor,
        thought_ctor,
        tool_use_ctor,
        diff_ctor,
    ) = sections::build_sections(ctx, senders.transcript.clone())?;
    stash.set("transcript", transcript_proxy)?;
    stash.set("MarkdownSection", md_ctor)?;
    stash.set("ErrorSection", err_ctor)?;
    stash.set("JsonSection", json_ctor)?;
    stash.set("ShellOutputSection", shell_output_ctor)?;
    stash.set("ReasoningSection", thought_ctor)?;
    stash.set("ToolUseSection", tool_use_ctor)?;
    stash.set("DiffSection", diff_ctor)?;

    let (chat_ctor, chat_inner_stream) =
        chat::build_chat_session_ctor(ctx, deps.clone(), senders.usage.clone())?;
    stash.set("ChatSession", chat_ctor)?;
    stash.set("__chat_inner_stream", chat_inner_stream)?;
    stash.set("__complete", chat::build_complete_fn(ctx, deps.clone())?)?;

    let shell_ctor = shell::build_shell_ctor(ctx, deps.clone())?;
    stash.set("Shell", shell_ctor)?;

    let editor_ctor = file::build_editor_ctor(ctx, deps.clone())?;
    stash.set("Editor", editor_ctor)?;

    let file_search_ctor = file_find_or_grep::build_file_search_ctor(ctx, deps.clone())?;
    stash.set("FileSearch", file_search_ctor)?;

    let jaq_eval = jaq::build_jaq_eval(ctx)?;
    stash.set("_jaqEval", jaq_eval)?;

    let (set_sleep, clear_sleep) =
        io::build_sleep_primitives(ctx, deps.timer().clone(), closed.clone())?;
    stash.set("_setSleep", set_sleep)?;
    stash.set("_clearSleep", clear_sleep)?;

    let approve_fn = permission::build_approve_primitive(ctx, approval_permissions_tx, closed)?;
    stash.set("_approve", approve_fn)?;

    let db_instance = storage::build_storage(ctx, workflow_db)?;
    stash.set("db", db_instance)?;
    let (discover_global, discover_local, discover_nested) =
        agents::build_agents_functions(ctx, deps.clone())?;
    stash.set("_discoverGlobalAgents", discover_global)?;
    stash.set("_discoverLocalAgents", discover_local)?;
    stash.set("_discoverNestedAgents", discover_nested)?;

    ctx.globals().set(STASH_KEY, stash)?;
    Ok(lifecycle_obj)
}

/// Declares the `whatwg:*` polyfill modules. Evaluation order matters:
/// `whatwg:abortcontroller` imports `DOMException` from `whatwg:dom`,
/// and captures `_setSleep` from the install stash (which must already
/// be live).
pub(crate) fn install_whatwg<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    declare_and_eval(ctx, "whatwg:dom")?;
    declare_and_eval(ctx, "whatwg:web-streams")?;
    declare_and_eval(ctx, "whatwg:abortcontroller")?;
    Ok(())
}

/// Declares + evaluates the `frances:v1/*` virtual modules. The stash
/// must already be live so each module body can `const __s =
/// globalThis.__frances_v1_stash__` and capture its slots.
pub(crate) fn install_v1_modules<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    declare_and_eval(ctx, "frances:v1/workflow")?;
    declare_and_eval(ctx, "frances:v1/lifecycle")?;
    declare_and_eval(ctx, "frances:v1/inbox")?;
    declare_and_eval(ctx, "frances:v1/sections")?;
    declare_and_eval(ctx, "frances:v1/chat")?;
    declare_and_eval(ctx, "frances:v1/io")?;
    declare_and_eval(ctx, "frances:v1/approval")?;
    declare_and_eval(ctx, "frances:v1/storage")?;
    declare_and_eval(ctx, "frances:v1/tool-family")?;
    declare_and_eval(ctx, "frances:v1/tools/shell")?;
    declare_and_eval(ctx, "frances:v1/tools/file")?;
    declare_and_eval(ctx, "frances:v1/tools/file_find_or_grep")?;
    declare_and_eval(ctx, "frances:v1/tools/variable")?;

    declare_and_eval(ctx, "frances:v1/context-sections")?;
    declare_and_eval(ctx, "frances:v1/agents")?;
    declare_and_eval(ctx, "frances:v1/agent-sections")?;

    Ok(())
}

/// Removes the install-time stash from `globalThis`. Must run after
/// every virtual module has been evaluated and captured its slots,
/// otherwise the bindings inside module scope become stale lookups.
pub(crate) fn remove_stash<'js>(ctx: &Ctx<'js>) -> Result<(), WorkflowError> {
    ctx.globals().remove(STASH_KEY)?;
    Ok(())
}

/// Build a JS `Error` from `message` and return it as the thrown `rquickjs::Error`.
pub(super) fn throw_js<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match rquickjs::Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}

/// `throw_js` pre-packaged as an `Err` for fns that return a JS `Result<T>`.
pub(super) fn throw_js_type<'js, T>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Result<T> {
    Err(throw_js(ctx, message))
}

/// Read a property, treating a missing/erroring lookup as `undefined`.
pub(super) fn get_or_undefined<'js>(ctx: &Ctx<'js>, obj: &Object<'js>, key: &str) -> Value<'js> {
    obj.get(key)
        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()))
}

/// Recursively convert an `rquickjs::Value` into a `serde_json::Value`.
/// Cheaper than going through the `Value -> String -> Value` round-trip,
/// and means JS-side arg shapes map 1:1 to whatever struct the caller
/// is deserialising into. Shared by `file::edit_inner` (LlmEdit args)
/// and `file_find_or_grep::do_search` (FileSearchArgs).
pub(super) fn rquickjs_to_json(value: &Value<'_>) -> Result<serde_json::Value, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(b) = value.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(i) = value.as_int() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Some(f) = value.as_float() {
        return Ok(serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null));
    }
    if let Some(s) = value.as_string() {
        return s
            .to_string()
            .map(serde_json::Value::String)
            .map_err(|e| format!("string conversion: {e}"));
    }
    if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr.iter::<Value<'_>>() {
            let item = item.map_err(|e| format!("array iter: {e}"))?;
            out.push(rquickjs_to_json(&item)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    if let Some(obj) = value.as_object() {
        let mut map = serde_json::Map::new();
        for entry in obj.props::<String, Value<'_>>() {
            let (k, v) = entry.map_err(|e| format!("object props: {e}"))?;
            map.insert(k, rquickjs_to_json(&v)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    Err("unsupported JS value type".to_owned())
}

/// Declares and evaluates a virtual module. Our modules are
/// synchronous (no top-level await) so the promise from `eval` is
/// fulfilled immediately and we can discard it — the export bindings
/// are valid as soon as `eval` returns.
fn declare_and_eval<'js>(
    ctx: &Ctx<'js>,
    name: &'static str,
) -> Result<rquickjs::module::Module<'js, rquickjs::module::Evaluated>, WorkflowError> {
    let source =
        load_asset_text(name).ok_or_else(|| WorkflowError::Script(JsError::new_loading(name)))?;
    let module = Module::declare(ctx.clone(), name, source.as_bytes())
        .catch(ctx)
        .map_err(caught(name))?;
    let (evaluated, _promise) = module.eval().catch(ctx).map_err(caught(name))?;
    Ok(evaluated)
}
