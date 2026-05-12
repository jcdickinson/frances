use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use tokio::net::UnixStream;
use tracing::{trace, warn};

use frances_llm::ChatSession;
use frances_models_llm::chat::ChatSession as ChatSessionTrait;
use frances_models_llm::wire::StreamEvent;

use crate::Result;
use crate::protocol::{BlockId, BlockKind, StreamFrame};
use crate::server::ServerChatDeps;
use crate::tools;
use crate::transport::{TransportError, write_message};
use crate::workflows;

use super::{ServerError, ServerState};

pub(super) async fn run_prompt(state: Arc<ServerState>, mut stream: UnixStream, text: String) {
    if let Err(error) = stream_prompt(&state, &mut stream, text).await {
        warn!(%error, "prompt handler failed");
        match write_message(&mut stream, &StreamFrame::Error(format!("{error}"))).await {
            Ok(()) => trace!("wrote error frame"),
            Err(e) => warn!(error = %e, "failed to write error frame"),
        }
    }
    match write_message(&mut stream, &StreamFrame::Done).await {
        Ok(()) => trace!("wrote done frame"),
        Err(e) => warn!(error = %e, "failed to write done frame"),
    }
}

async fn stream_prompt(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: String,
) -> Result<()> {
    let result = run_handler(state, stream, text).await;
    if let Err(error) = state.edit_session.lock().await.end_turn().await {
        warn!(%error, "edit_session::end_turn failed");
    }
    result
}

async fn run_handler(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: String,
) -> Result<()> {
    workflows::cycle(state, stream, &text).await
}

/// The legacy Rust-driven LLM chat turn. Invoked from the workflow
/// stack's bottom frame (`LegacyLlmTurn`); will go away once the chat
/// loop is ported into a JS workflow.
pub(crate) async fn run_legacy_llm_turn(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: &str,
) -> Result<()> {
    let (env, cwd) = {
        let guard = state.last_context.lock();
        let ctx = guard.as_ref().ok_or(ServerError::NoClientContext)?;
        (ctx.process.env.clone(), ctx.process.cwd.clone())
    };

    let chat = state.primary_chat.clone();

    let mut next_block: u64 = 1;
    let mut alloc_block = || {
        let id = BlockId(next_block);
        next_block += 1;
        id
    };

    let mut send_error: Option<TransportError> = None;

    chat.submit_user(text).await?;

    let user_id = alloc_block();
    for frame in [
        StreamFrame::BlockStart {
            id: user_id,
            kind: BlockKind::UserText,
        },
        StreamFrame::BlockDelta {
            id: user_id,
            text: text.to_owned(),
        },
        StreamFrame::BlockStop { id: user_id },
    ] {
        try_write(stream, &frame, &mut send_error).await;
    }

    let mut iterations: u32 = 0;
    loop {
        iterations += 1;
        if iterations.is_multiple_of(25) {
            warn!(iterations, "agent loop running long");
        }

        let made_tool_calls = run_llm_step(
            state,
            stream,
            &chat,
            &env,
            &mut alloc_block,
            &mut send_error,
            cwd.as_deref(),
        )
        .await?;

        if !made_tool_calls {
            break;
        }
    }

    if let Some(error) = send_error {
        return Err(ServerError::Send(error).into());
    }
    Ok(())
}

/// Runs one LLM call, streams the result, persists the assistant message,
/// and dispatches any tool calls (also persisting their results). Returns
/// `true` if the model emitted tool calls (caller should loop), `false` if
/// the model's response was terminal.
async fn run_llm_step(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    chat: &ChatSession<ServerChatDeps>,
    env: &HashMap<OsString, OsString>,
    alloc_block: &mut impl FnMut() -> BlockId,
    send_error: &mut Option<TransportError>,
    cwd: Option<&std::path::Path>,
) -> Result<bool> {
    let assistant_id = alloc_block();
    let mut wire_active: Option<BlockId> = Some(assistant_id);
    try_write(
        stream,
        &StreamFrame::BlockStart {
            id: assistant_id,
            kind: BlockKind::AssistantText,
        },
        send_error,
    )
    .await;

    let tool_defs = state.tool_registry.definitions().await?;

    let chat_for_task = chat.clone();
    let env_for_task = env.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
    let llm_task = tokio::spawn(async move {
        ChatSessionTrait::run(
            &chat_for_task,
            env_for_task,
            tool_defs,
            None,
            Box::new(move |event: StreamEvent| {
                let _ = tx.send(event);
                Ok(())
            }),
        )
        .await
    });

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => {
                if wire_active == Some(assistant_id) {
                    try_write(
                        stream,
                        &StreamFrame::BlockDelta {
                            id: assistant_id,
                            text: delta,
                        },
                        send_error,
                    )
                    .await;
                }
            }
            StreamEvent::ToolCall(call) => {
                if let Some(active) = wire_active.take() {
                    try_write(stream, &StreamFrame::BlockStop { id: active }, send_error).await;
                }
                let block_id = alloc_block();
                wire_active = Some(block_id);
                try_write(
                    stream,
                    &StreamFrame::BlockStart {
                        id: block_id,
                        kind: BlockKind::ToolUse {
                            name: call.name.clone(),
                        },
                    },
                    send_error,
                )
                .await;
                try_write(
                    stream,
                    &StreamFrame::BlockDelta {
                        id: block_id,
                        text: call.arguments.to_string(),
                    },
                    send_error,
                )
                .await;
            }
            StreamEvent::Usage(usage) => {
                try_write(stream, &StreamFrame::Usage(usage), send_error).await;
            }
            StreamEvent::History(_) => {
                // ChatSession::run consumes History events internally; this
                // arm exists only to keep the match exhaustive.
            }
        }
    }

    if let Some(id) = wire_active.take() {
        try_write(stream, &StreamFrame::BlockStop { id }, send_error).await;
    }

    let stream_result = llm_task.await.map_err(ServerError::LlmTaskPanicked)?;
    let outcome = stream_result?;
    let tool_calls = outcome.tool_calls;

    if tool_calls.is_empty() {
        return Ok(false);
    }

    for call in &tool_calls {
        let outcome = state
            .tool_registry
            .dispatch(
                call,
                &tools::ToolContext {
                    edit_session: &state.edit_session,
                    cwd,
                },
            )
            .await;

        let result_id = alloc_block();
        try_write(
            stream,
            &StreamFrame::BlockStart {
                id: result_id,
                kind: BlockKind::ToolResult {
                    tool_use_id: call.id.clone(),
                    is_error: outcome.is_error,
                },
            },
            send_error,
        )
        .await;
        try_write(
            stream,
            &StreamFrame::BlockDelta {
                id: result_id,
                text: outcome.content.clone(),
            },
            send_error,
        )
        .await;
        try_write(
            stream,
            &StreamFrame::BlockStop { id: result_id },
            send_error,
        )
        .await;

        chat.submit_tool_result(&call.id, &outcome.content, outcome.is_error)
            .await?;
    }

    Ok(true)
}

/// Best-effort frame write that records the first send error and silently
/// no-ops afterward. Once the client socket is gone, further frames would
/// just fail; we keep consuming the LLM stream and persisting history so
/// the next attach can replay it.
async fn try_write(
    stream: &mut UnixStream,
    frame: &StreamFrame,
    send_error: &mut Option<TransportError>,
) {
    if send_error.is_some() {
        return;
    }
    if let Err(error) = write_message(stream, frame).await {
        *send_error = Some(error);
    }
}
