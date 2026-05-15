//! Per-session workflow stack.
//!
//! User input always dispatches to whatever's on top of the stack. The
//! stack starts empty; bootstrap pushes the configured `default_workflow`
//! (if any) before accepting input. Slash commands push fresh [`Frame`]s
//! on top; when a workflow terminates (top-level body settles or
//! `workflow.exit()`), the frame pops.
//!
//! Non-slash input on an empty stack returns a one-shot error frame
//! — there is no host-side fallback chat loop anymore.
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
    /// Builds an empty stack. Bootstrap pushes the configured
    /// `default_workflow` (if any) before the daemon starts accepting
    /// input.
    pub fn new() -> Self {
        Self {
            frames: AsyncMutex::new(Vec::new()),
        }
    }
}

/// One entry on the stack: a running JS workflow plus the wire-state
/// needed across multiple `drive()` invocations (block id allocator,
/// currently-open block). Popped when the workflow's body terminates.
struct Frame {
    handle: WorkflowHandle,
    emit: EmitState,
}

/// Block-tracking state for a single JS workflow's lifetime.
///
/// Workflow frames map to wire blocks like this:
///
/// - `MarkdownFrame` push: close previous open block (if any), open a
///   new `Text { sender }` block, write initial content; the block stays
///   open so subsequent `append`s stream into it. A block can outlive
///   the `Done` of its opening cycle — the UI doesn't auto-finalise on
///   Done, so the block keeps streaming across user-input turns until
///   a new push supersedes it or the workflow exits.
/// - `MarkdownFrame.append`: write a `BlockDelta` on the currently-open
///   block.
/// - `ErrorFrame` push: close previous open block, emit a one-shot
///   `StreamFrame::Error`.
/// - `JsonFrame` push: close previous open block, open + immediately
///   close a one-shot `Text { sender: None }` block rendering
///   `[tag] body`.
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

/// Start the workflow named `name` and push it onto the stack with no
/// initial drive. Used at daemon bootstrap to seat the configured
/// `default_workflow` before any client has attached — there is no
/// `UnixStream` to write to, so anything the workflow emits during its
/// top-level evaluation (e.g. a welcome `MarkdownFrame`) buffers in the
/// `WorkflowHandle::frames` channel and is flushed by the first
/// `dispatch_topmost` call when a client sends input.
///
/// Returns `Ok(false)` (and logs a warning) if `name` is not a key
/// under `[workflows.*]`; in that case the stack is left empty.
pub(crate) async fn push_default_workflow(state: &Arc<ServerState>, name: &str) -> Result<bool> {
    let workflows = state.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        warn!(
            workflow = name,
            "default_workflow is set but no matching [workflows.*] entry exists; \
             leaving stack empty"
        );
        return Ok(false);
    };

    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args: Vec::new(),
    };

    let handle = state.workflow_runtime.start(invocation)?;
    state.workflow_stack.frames.lock().await.push(Frame {
        handle,
        emit: EmitState::new(),
    });
    Ok(true)
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

    let mut frame = Frame {
        handle,
        emit: EmitState::new(),
    };
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
    // push it back. Empty stack ⇒ no default workflow configured.
    let mut top = {
        let mut stack = state.workflow_stack.frames.lock().await;
        match stack.pop() {
            Some(top) => top,
            None => {
                write_message(
                    stream,
                    &StreamFrame::Error(
                        "no default workflow configured; use a slash command \
                         or set `default_workflow` in your config"
                            .to_owned(),
                    ),
                )
                .await?;
                return Ok(());
            }
        }
    };

    // Sending to a dropped receiver would mean the body has already
    // exited and we just didn't observe it yet; treat that as "exited"
    // and let the drive loop confirm.
    let _ = top.handle.input_tx.send(UserInput {
        content: text.to_owned(),
    });
    let exited = drive(&mut top, stream).await?;
    if !exited {
        state.workflow_stack.frames.lock().await.push(top);
    }
    Ok(())
}

/// Drains the frame's host-frame channel until the body either parks
/// waiting for input or terminates. Returns `true` if the body exited.
async fn drive(frame: &mut Frame, stream: &mut UnixStream) -> Result<bool> {
    loop {
        while let Ok(host_frame) = frame.handle.frames.try_recv() {
            emit(stream, &mut frame.emit, host_frame).await?;
        }
        tokio::select! {
            biased;
            Some(host_frame) = frame.handle.frames.recv() => {
                emit(stream, &mut frame.emit, host_frame).await?;
            }
            done = &mut frame.handle.done => {
                while let Ok(host_frame) = frame.handle.frames.try_recv() {
                    emit(stream, &mut frame.emit, host_frame).await?;
                }
                // Workflow is terminating — make sure any open block is
                // closed before we surface the result.
                frame.emit.close_open(stream).await?;
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
            () = frame.handle.parked.notified() => {
                while let Ok(host_frame) = frame.handle.frames.try_recv() {
                    emit(stream, &mut frame.emit, host_frame).await?;
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
            FrameKind::Markdown { content, sender } => {
                state.close_open(stream).await?;
                let block = state.alloc();
                write_message(
                    stream,
                    &StreamFrame::BlockStart {
                        id: block,
                        kind: BlockKind::Text { sender },
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
                        kind: BlockKind::Text { sender: None },
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
