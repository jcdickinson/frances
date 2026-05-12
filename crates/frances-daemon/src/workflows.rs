//! Per-session workflow stack.
//!
//! User input always dispatches to whatever's on top of the stack. The
//! stack is bootstrapped with a single [`Frame::LegacyLlmTurn`] at the
//! bottom — that frame is the existing Rust-driven chat loop and is
//! never popped. Slash commands push new [`Frame::Js`] frames on top;
//! when a JS workflow terminates (top-level body settles or
//! `workflow.exit()`), the frame pops.
//!
//! This module owns the dispatch glue between the daemon's wire
//! protocol ([`StreamFrame`], the client `UnixStream`) and the workflow
//! runtime in [`frances_workflow`].

use std::sync::Arc;

use tokio::net::UnixStream;
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

use crate::Result;
use crate::protocol::{BlockId, BlockKind, StreamFrame};
use crate::server::ServerState;
use crate::transport::write_message;

use frances_workflow::{
    FrameKind, FramePush, HostFrame, Invocation, UserInput, WorkflowHandle, parse_slash_command,
};
pub use frances_workflow::{Runtime as WorkflowRuntime, WorkflowConfig, WorkflowError};

/// The session-scoped workflow stack. One per `ServerState`.
pub struct WorkflowStack {
    frames: AsyncMutex<Vec<Frame>>,
}

impl Default for WorkflowStack {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowStack {
    /// Builds a fresh stack with the legacy chat-loop frame at the
    /// bottom. That frame stays for the daemon's lifetime.
    pub fn new() -> Self {
        Self {
            frames: AsyncMutex::new(vec![Frame::LegacyLlmTurn]),
        }
    }
}

/// One entry on the stack.
enum Frame {
    /// Today's Rust-driven LLM chat turn. Wraps [`run_legacy_llm_turn`].
    /// Never popped.
    LegacyLlmTurn,
    /// A running JS workflow. Popped when the body terminates.
    Js(JsFrame),
}

/// A running JS workflow plus the wire-state needed across multiple
/// `drive()` invocations (block id allocator, currently-open block).
struct JsFrame {
    handle: WorkflowHandle,
    emit: EmitState,
}

/// Block-tracking state for a single JS workflow's lifetime.
///
/// Workflow frames map to wire blocks like this:
///
/// - `MarkdownFrame` push: close previous open block (if any), open a
///   new `AssistantText` block, write initial content; the block stays
///   open so subsequent `append`s stream into it. A block can outlive
///   the `Done` of its opening cycle — the UI doesn't auto-finalise on
///   Done, so the block keeps streaming across user-input turns until
///   a new push supersedes it or the workflow exits.
/// - `MarkdownFrame.append`: write a `BlockDelta` on the currently-open
///   block.
/// - `ErrorFrame` push: close previous open block, emit a one-shot
///   `StreamFrame::Error`.
/// - `JsonFrame` push: close previous open block, open + immediately
///   close a one-shot `AssistantText` block rendering `[tag] body`.
///
/// On workflow termination the open block is closed before `Done` so
/// the UI's `BlockState` ends up Idle.
struct EmitState {
    next_block: u64,
    open_block: Option<BlockId>,
}

impl EmitState {
    fn new() -> Self {
        Self {
            next_block: 1,
            open_block: None,
        }
    }

    fn alloc(&mut self) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        id
    }

    async fn close_open(&mut self, stream: &mut UnixStream) -> Result<()> {
        if let Some(id) = self.open_block.take() {
            write_message(stream, &StreamFrame::BlockStop { id }).await?;
        }
        Ok(())
    }
}

/// Top-level entry from the prompt RPC. Parses the input and either
/// pushes a fresh JS workflow (for slash commands) or hands the text to
/// the topmost frame. Always finishes one "cycle" — i.e. drives the
/// topmost frame until it parks waiting for input or terminates.
pub(crate) async fn cycle(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: &str,
) -> Result<()> {
    match parse_slash_command(text) {
        Ok(Some((name, args))) => {
            let name = name.to_owned();
            push_and_drive(state, stream, &name, args).await
        }
        Ok(None) => dispatch_topmost(state, stream, text).await,
        Err(error) => {
            write_message(
                stream,
                &StreamFrame::Error(format!("bad workflow args: {error}")),
            )
            .await?;
            Ok(())
        }
    }
}

async fn push_and_drive(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    name: &str,
    args: Vec<String>,
) -> Result<()> {
    let workflows = state.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        write_message(
            stream,
            &StreamFrame::Error(format!("unknown workflow: {name}")),
        )
        .await?;
        return Ok(());
    };

    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args,
    };

    let handle = match state.workflow_runtime.start(invocation) {
        Ok(handle) => handle,
        Err(error) => {
            write_message(stream, &StreamFrame::Error(format!("workflow: {error}"))).await?;
            return Ok(());
        }
    };

    let mut frame = Frame::Js(JsFrame {
        handle,
        emit: EmitState::new(),
    });
    let exited = drive(&mut frame, stream).await?;
    if !exited {
        state.workflow_stack.frames.lock().await.push(frame);
    }
    Ok(())
}

async fn dispatch_topmost(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: &str,
) -> Result<()> {
    // Pop the topmost frame so we can hand it to `drive` without
    // holding the stack lock across the drain. If it stays alive we
    // push it back; the legacy frame is always re-pushed.
    let mut top = {
        let mut stack = state.workflow_stack.frames.lock().await;
        stack.pop().expect("stack is never empty")
    };

    let outcome = match &mut top {
        Frame::LegacyLlmTurn => {
            run_legacy_llm_turn(state, stream, text).await?;
            DriveOutcome::Continue
        }
        Frame::Js(js) => {
            // Sending to a dropped receiver would mean the body has
            // already exited and we just didn't observe it yet; treat
            // that as "exited" and let the drive loop confirm.
            let _ = js.handle.input_tx.send(UserInput {
                content: text.to_owned(),
            });
            if drive(&mut top, stream).await? {
                DriveOutcome::Exited
            } else {
                DriveOutcome::Continue
            }
        }
    };

    if matches!(outcome, DriveOutcome::Continue) {
        state.workflow_stack.frames.lock().await.push(top);
    }
    Ok(())
}

enum DriveOutcome {
    Continue,
    Exited,
}

/// Drains the frame's host-frame channel until the body either parks
/// waiting for input or terminates. Returns `true` if the body exited.
async fn drive(frame: &mut Frame, stream: &mut UnixStream) -> Result<bool> {
    let Frame::Js(js) = frame else {
        // LegacyLlmTurn is driven by `run_legacy_llm_turn` directly;
        // it never reaches this path.
        return Ok(false);
    };

    loop {
        while let Ok(host_frame) = js.handle.frames.try_recv() {
            emit(stream, &mut js.emit, host_frame).await?;
        }
        tokio::select! {
            biased;
            Some(host_frame) = js.handle.frames.recv() => {
                emit(stream, &mut js.emit, host_frame).await?;
            }
            done = &mut js.handle.done => {
                while let Ok(host_frame) = js.handle.frames.try_recv() {
                    emit(stream, &mut js.emit, host_frame).await?;
                }
                // Workflow is terminating — make sure any open block is
                // closed before we surface the result.
                js.emit.close_open(stream).await?;
                if let Ok(Err(error)) = done {
                    write_message(
                        stream,
                        &StreamFrame::Error(format!("workflow: {error}")),
                    )
                    .await?;
                } else if let Err(error) = done {
                    warn!(%error, "workflow done channel closed without value");
                }
                return Ok(true);
            }
            () = js.handle.parked.notified() => {
                while let Ok(host_frame) = js.handle.frames.try_recv() {
                    emit(stream, &mut js.emit, host_frame).await?;
                }
                // The open block stays open across the cycle boundary
                // — the UI doesn't finalise on Done, so the frame keeps
                // streaming naturally into the next user turn.
                return Ok(false);
            }
        }
    }
}

async fn emit(stream: &mut UnixStream, state: &mut EmitState, frame: HostFrame) -> Result<()> {
    match frame {
        HostFrame::Push(FramePush { id: _, kind }) => match kind {
            FrameKind::Markdown { content } => {
                state.close_open(stream).await?;
                let block = state.alloc();
                write_message(
                    stream,
                    &StreamFrame::BlockStart {
                        id: block,
                        kind: BlockKind::AssistantText,
                    },
                )
                .await?;
                write_message(
                    stream,
                    &StreamFrame::BlockDelta {
                        id: block,
                        text: content,
                    },
                )
                .await?;
                state.open_block = Some(block);
            }
            FrameKind::Error { content } => {
                state.close_open(stream).await?;
                write_message(stream, &StreamFrame::Error(content)).await?;
            }
            FrameKind::Json { tag, value } => {
                state.close_open(stream).await?;
                let body =
                    serde_json::to_string(&value).unwrap_or_else(|_| "<unserializable>".into());
                let block = state.alloc();
                write_message(
                    stream,
                    &StreamFrame::BlockStart {
                        id: block,
                        kind: BlockKind::AssistantText,
                    },
                )
                .await?;
                write_message(
                    stream,
                    &StreamFrame::BlockDelta {
                        id: block,
                        text: format!("[{tag}] {body}"),
                    },
                )
                .await?;
                write_message(stream, &StreamFrame::BlockStop { id: block }).await?;
            }
        },
        HostFrame::Append { delta, .. } => {
            if let Some(id) = state.open_block {
                write_message(stream, &StreamFrame::BlockDelta { id, text: delta }).await?;
            }
            // else: no appendable block open — JS guarantees this only
            // happens if push was never called, which would have
            // thrown before reaching here.
        }
        HostFrame::Approval(request) => {
            // Close any open block first. The TUI commits its active
            // block to scrollback when it shows the approval prompt
            // (state.take() in ui.rs), so leaving the block tracked
            // here would desync the two: the next BlockStop we emit
            // (when the workflow body pushes its next frame) would
            // arrive while the UI is Idle. Closing here keeps the
            // wire's open_block in lockstep with the UI.
            state.close_open(stream).await?;
            write_message(stream, &StreamFrame::Approval(request)).await?;
        }
    }
    Ok(())
}

/// Runs one turn through the legacy Rust chat loop. Delegates back to
/// the [`turn`](crate::server) module so the LLM-loop code stays in one
/// place; the stack wraps it as the bottom frame.
async fn run_legacy_llm_turn(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: &str,
) -> Result<()> {
    crate::server::run_legacy_llm_turn(state, stream, text).await
}
