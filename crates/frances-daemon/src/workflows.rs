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

use frances_workflow::{HostFrame, Invocation, UserInput, WorkflowHandle, parse_slash_command};
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
    Js(WorkflowHandle),
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

    let mut frame = Frame::Js(handle);
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
        Frame::Js(handle) => {
            // Sending to a dropped receiver would mean the body has
            // already exited and we just didn't observe it yet; treat
            // that as "exited" and let the drive loop confirm.
            let _ = handle.input_tx.send(UserInput {
                message: text.to_owned(),
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
    let Frame::Js(handle) = frame else {
        // LegacyLlmTurn is driven by `run_legacy_llm_turn` directly;
        // it never reaches this path.
        return Ok(false);
    };

    let mut next_block: u64 = 1;
    let mut alloc = || {
        let id = BlockId(next_block);
        next_block += 1;
        id
    };

    loop {
        while let Ok(host_frame) = handle.frames.try_recv() {
            emit(stream, &mut alloc, host_frame).await?;
        }
        tokio::select! {
            biased;
            Some(host_frame) = handle.frames.recv() => {
                emit(stream, &mut alloc, host_frame).await?;
            }
            done = &mut handle.done => {
                while let Ok(host_frame) = handle.frames.try_recv() {
                    emit(stream, &mut alloc, host_frame).await?;
                }
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
            () = handle.parked.notified() => {
                while let Ok(host_frame) = handle.frames.try_recv() {
                    emit(stream, &mut alloc, host_frame).await?;
                }
                return Ok(false);
            }
        }
    }
}

async fn emit(
    stream: &mut UnixStream,
    alloc: &mut impl FnMut() -> BlockId,
    frame: HostFrame,
) -> Result<()> {
    match frame {
        HostFrame::Text(text) => write_text_block(stream, alloc(), text).await?,
        HostFrame::Error(text) => write_message(stream, &StreamFrame::Error(text)).await?,
        HostFrame::Json { tag, value } => {
            // No structured frame variant for tagged JSON yet; render
            // it as assistant text so it at least surfaces in the UI.
            // Replace with a typed frame when the gate/recall surfaces
            // need machine-readable workflow events.
            let body = serde_json::to_string(&value).unwrap_or_else(|_| "<unserializable>".into());
            write_text_block(stream, alloc(), format!("[{tag}] {body}")).await?;
        }
    }
    Ok(())
}

async fn write_text_block(stream: &mut UnixStream, id: BlockId, text: String) -> Result<()> {
    write_message(
        stream,
        &StreamFrame::BlockStart {
            id,
            kind: BlockKind::AssistantText,
        },
    )
    .await?;
    write_message(stream, &StreamFrame::BlockDelta { id, text }).await?;
    write_message(stream, &StreamFrame::BlockStop { id }).await?;
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
