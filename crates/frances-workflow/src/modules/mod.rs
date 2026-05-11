//! Virtual modules exposed to workflow scripts under `frances:v1/*`.
//!
//! Each invocation gets a fresh QuickJS context (`AsyncContext::full`).
//! Before evaluating the user script, we build the per-invocation host
//! values (a `Function` for `exit`, a `Class` instance for `inbox`, the
//! transcript proxy, the frame-class constructors) and declare a small
//! virtual module for each one. Those modules just re-export the values
//! out of a hidden global stash — keeping all the per-invocation state
//! inside the captured closures, not in any runtime-wide map.
//!
//! The stash key lives on `globalThis` for the lifetime of the
//! invocation's context. The context is discarded at end of invocation,
//! so there's no leak across runs. It's not designed to defend against
//! an adversarial workflow author — workflows are local code, not
//! untrusted input.
//!
//! Modules:
//!
//! - `frances:v1/workflow` — `exit` lifecycle function.
//! - `frances:v1/inbox`    — `inbox` async-iterable user-input stream.
//! - `frances:v1/frames`   — `transcript`, `MarkdownFrame`, `ErrorFrame`,
//!   `JsonFrame` (frame-objects-with-history API).
//! - `frances:v1/chat`     — `ChatSession` (LLM access). Constructor
//!   currently throws — see `chat.rs`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rquickjs::module::Module;
use rquickjs::{CatchResultExt, Ctx, Object};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::WorkflowError;
use crate::runtime::{HostFrame, UserInput};

pub mod chat;
pub mod frames;
pub mod inbox;
pub mod workflow;

/// Global key on `globalThis` where the module stash lives during a
/// workflow's lifetime. Cleared just before the user script is declared
/// so workflows can't reach it from JS.
const STASH_KEY: &str = "__frances_v1_stash__";

/// Wires the `frances:v1/*` virtual modules into `ctx`. Builds the
/// per-invocation host values (closures over the channels/flags),
/// stashes them on `globalThis`, and declares each virtual module.
pub(crate) fn install_v1<'js>(
    ctx: &Ctx<'js>,
    frames_tx: UnboundedSender<HostFrame>,
    input_rx: Arc<AsyncMutex<UnboundedReceiver<UserInput>>>,
    closed: Arc<AtomicBool>,
    closed_notify: Arc<Notify>,
    parked: Arc<Notify>,
) -> Result<(), WorkflowError> {
    let stash = Object::new(ctx.clone()).map_err(script)?;

    let exit_fn =
        workflow::build_exit(ctx, closed.clone(), closed_notify.clone()).map_err(script)?;
    stash.set("exit", exit_fn).map_err(script)?;

    let inbox_instance =
        inbox::build_inbox(ctx, input_rx, closed.clone(), closed_notify.clone(), parked)
            .map_err(script)?;
    stash.set("inbox", inbox_instance).map_err(script)?;

    let (transcript_proxy, md_ctor, err_ctor, json_ctor) =
        frames::build_frames(ctx, frames_tx).map_err(script)?;
    stash.set("transcript", transcript_proxy).map_err(script)?;
    stash.set("MarkdownFrame", md_ctor).map_err(script)?;
    stash.set("ErrorFrame", err_ctor).map_err(script)?;
    stash.set("JsonFrame", json_ctor).map_err(script)?;

    let chat_ctor = chat::build_chat_session_ctor(ctx).map_err(script)?;
    stash.set("ChatSession", chat_ctor).map_err(script)?;

    ctx.globals().set(STASH_KEY, stash).map_err(script)?;

    declare(ctx, "frances:v1/workflow", WORKFLOW_SRC)?;
    declare(ctx, "frances:v1/inbox", INBOX_SRC)?;
    declare(ctx, "frances:v1/frames", FRAMES_SRC)?;
    declare(ctx, "frances:v1/chat", CHAT_SRC)?;

    Ok(())
}

fn declare<'js>(ctx: &Ctx<'js>, name: &str, source: &str) -> Result<(), WorkflowError> {
    Module::declare(ctx.clone(), name, source)
        .catch(ctx)
        .map_err(|e| WorkflowError::Script(format!("declare {name}: {e}")))?;
    Ok(())
}

fn script<E: std::fmt::Display>(err: E) -> WorkflowError {
    WorkflowError::Script(err.to_string())
}

// ---- Module source strings ------------------------------------------------
//
// Each module just re-exports its slot from the stash. The stash is
// dropped from globalThis after declarations land — but by then the
// module bodies have captured the values via the `const s = ...;`
// binding at evaluation time. (Modules evaluate when imported.)

const WORKFLOW_SRC: &str = r#"
const __s = globalThis.__frances_v1_stash__;
export const exit = __s.exit;
"#;

const INBOX_SRC: &str = r#"
const __s = globalThis.__frances_v1_stash__;
export const inbox = __s.inbox;
"#;

const FRAMES_SRC: &str = r#"
const __s = globalThis.__frances_v1_stash__;
export const transcript = __s.transcript;
export const MarkdownFrame = __s.MarkdownFrame;
export const ErrorFrame = __s.ErrorFrame;
export const JsonFrame = __s.JsonFrame;
"#;

const CHAT_SRC: &str = r#"
const __s = globalThis.__frances_v1_stash__;
export const ChatSession = __s.ChatSession;
"#;
