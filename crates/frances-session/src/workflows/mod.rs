//! Per-session workflow stack.
//!
//! ## Single-slot in-memory, multi-level in DB
//!
//! At any time only one workflow runs — the **top** of the stack
//! ([`WorkflowStack::top`]). Levels below the top live as rows in the
//! `workflow_stack` table (`active = 0`, `completed_at IS NULL`). When
//! a slash command pushes B on top of A, A is **dehydrated**:
//! [`WorkflowHandle::request_shutdown`] fires, the body's
//! `frances:v1/lifecycle` hook runs, the inbox closes, and A's task
//! ends. A's row stays in the DB. When B exits, A is **rehydrated**:
//! a fresh runtime starts with A's original `instance_id` round-tripped
//! into `import.meta.instance`, so A's body can read its own table
//! state and pick up where it left off.
//!
//! ## Append-only table
//!
//! Pops never delete: they set `completed_at` and clear `active`.
//! Push truncates any non-completed rows above the current top (a
//! defensive sweep against crash-mid-pop) and inserts the new row
//! with the next AUTOINCREMENT position. Rows accumulate; a future
//! "resume previously-popped" feature is unblocked at the schema
//! level but not built here.
//!
//! ## Boot
//!
//! [`restore_or_seed`] reads the table. If `COUNT(*) = 0`, it pushes
//! the configured `default_workflow` (which inserts the first row and
//! hydrates). Otherwise it hydrates the row with `active = 1`. If no
//! row is active (the user popped everything down to zero live rows
//! in a previous session), the stack starts empty — the default
//! workflow is **not** re-seeded.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;
use turso::Value;
use uuid::Uuid;

use crate::Result;
use crate::events::{BlockId, BlockKind, StreamFrame};
use crate::runtime::{EventsChannel, SessionRuntime};
use crate::store::Database;


use frances_storage::{EntitySchema, Migration};
use frances_workflow::{
    FrameId, FrameKind, FramePush, HostFrame, Invocation, UserInput, WorkflowHandle,
    parse_slash_command,
};
pub use frances_workflow::{Runtime as WorkflowRuntime, WorkflowConfig, WorkflowError};

/// Owns the per-session `workflow_stack` table. UUID is permanent;
/// never edit.
pub static SCHEMA: EntitySchema<'static> = EntitySchema {
    entity: Uuid::from_u128(0x6f3a8c1d_0b4e_4b9a_9c1f_5d8a2e6f7b30),
    migrations: Cow::Borrowed(&[Migration {
        name: Cow::Borrowed("0001_init.sql"),
        sql: Cow::Borrowed(include_str!("migrations/0001_init.sql")),
    }]),
};

/// Errors specific to the workflow-stack persistence layer. Wraps
/// turso + JSON decoding failures; emitted as
/// [`crate::Error::WorkflowStack`] so callers can `?` through
/// `crate::Result`.
#[derive(Debug, Error)]
pub enum WorkflowStackError {
    #[error("workflow_stack sql: {0}")]
    Turso(#[from] turso::Error),
    #[error("workflow_stack: malformed args json: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "workflow_stack: unexpected column shape for {column}: expected {expected}, got {found:?}"
    )]
    UnexpectedColumn {
        column: &'static str,
        expected: &'static str,
        found: Value,
    },
    #[error("workflow_stack: instance_id is not 16 bytes (got {got})")]
    InstanceIdLength { got: usize },
}

/// How long to wait for a dehydrating workflow's body to settle after
/// `request_shutdown`. Bounded so a misbehaving `lifecycle.shutdown`
/// hook can't hang a push.
const DEHYDRATE_TIMEOUT: Duration = Duration::from_secs(5);

/// The session-scoped workflow stack. Holds the currently-hydrated
/// workflow (if any) plus the per-session [`Database`] used for stack
/// persistence.
pub struct WorkflowStack {
    top: AsyncMutex<Option<WorkflowInstance>>,
    db: Database,
}

impl WorkflowStack {
    /// Builds an empty in-memory stack bound to `db`. Layering across
    /// daemon restarts lives entirely in the per-session
    /// `workflow_stack` table on this database.
    pub fn new(db: Database) -> Self {
        Self {
            top: AsyncMutex::new(None),
            db,
        }
    }

    /// Per-session [`Database`] handle. Same lock the rest of the daemon
    /// uses; cheap clone for callers (like scrollback replay) that want
    /// to issue SQL against it without going through the stack itself.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// `instance_id` of the currently-hydrated workflow, if any.
    /// Read-only helper for callers (e.g. attach) that need to know
    /// which workflow to replay scrollback for.
    pub async fn active_instance(&self) -> Option<Uuid> {
        self.top.lock().await.as_ref().map(|i| i.handle.instance)
    }
}

/// The currently-hydrated workflow plus the wire-state needed across
/// multiple `drive()` invocations (block id allocator, currently-open
/// block) and a copy of its `config_key` for diagnostics. The runtime
/// `WorkflowHandle` already carries the `instance_id`.
struct WorkflowInstance {
    handle: WorkflowHandle,
    emit: EmitState,
    #[expect(
        dead_code,
        reason = "useful in tracing/logging; not yet read at any call site"
    )]
    config_key: String,
}

/// Block-tracking state for a single hydrated workflow's lifetime.
///
/// Workflow frames map to wire blocks like this:
///
/// - `MarkdownFrame` push: open a new `Text { sender }` block, write
///   initial content; the block stays open so subsequent `append`s
///   stream into it. The JS side sends `Close` for the prior active
///   markdown before pushing a new one — multiple blocks can be open
///   concurrently (e.g. a shell-output block running while the LLM
///   streams text into a markdown block above it).
/// - `MarkdownFrame.append`: `Append` carries the [`FrameId`]; the
///   emit state looks up the matching open block and writes a
///   `BlockDelta` against it.
/// - `Close { id }`: emit `BlockStop` for the block, persist a clean
///   scrollback row, remove from `open`.
/// - `UpdateKind { id, kind }`: emit a no-text `BlockDelta` carrying
///   the new kind. Used for in-place metadata transitions (ShellOutput
///   Running → Success/Exit(N)).
/// - `ErrorFrame` push: emit a one-shot `StreamFrame::Error` and
///   persist a scrollback row of kind 'error'. Does NOT touch any
///   open block — error frames are side-channel.
/// - `JsonFrame` push: open + immediately close a one-shot
///   `Text { sender: None }` block rendering `[tag] body`.
///
/// On workflow termination every remaining open block is closed so the
/// UI's `BlockState` ends up Idle. `EmitState` accumulates the delta
/// text for each open block so we can persist the full body on close
/// — either clean (a `BlockStop`) or truncated (workflow was dehydrated
/// while a block was in flight).
struct EmitState {
    next_block: u64,
    /// Open blocks keyed by the workflow-side [`FrameId`]. Emit is
    /// single-threaded (one task per workflow instance) so a plain
    /// `HashMap` is enough.
    open: HashMap<FrameId, OpenBlock>,
    /// Shared per-session [`Database`] used for scrollback writes. Cheap
    /// clone; the underlying connection lock serialises overlapping
    /// writes.
    db: Database,
    /// Identifies the workflow whose blocks we're emitting. Every
    /// scrollback row written from this state is tagged with it so
    /// replay can scope by workflow.
    instance_id: Uuid,
}

/// A block whose first `BlockDelta` has been emitted but whose
/// `BlockStop` has not. We buffer the delta text here so that on close
/// we can write one scrollback row with the full body.
///
/// `text` is `None` for a block that was pushed without initial content
/// and has never received an `Append` — the client has only seen the
/// opener with `text: None` and is still deferring measure / render. On
/// `Close` we skip persistence for these so the transcript doesn't
/// gain empty ghost rows for never-written frames.
struct OpenBlock {
    id: BlockId,
    kind: BlockKind,
    text: Option<String>,
}

impl EmitState {
    fn new(db: Database, instance_id: Uuid) -> Self {
        Self {
            next_block: 1,
            open: HashMap::new(),
            db,
            instance_id,
        }
    }

    fn alloc(&mut self) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        id
    }

    /// Clean close for a single block: emit `BlockStop`, persist a
    /// finished row (if the block ever received body content), drop
    /// the entry. Idempotent on unknown ids.
    async fn close_one(&mut self, events: &EventsChannel, frame_id: FrameId) -> Result<()> {
        let Some(open) = self.open.remove(&frame_id) else {
            return Ok(());
        };
        events.send(StreamFrame::BlockStop { id: open.id });
        if let Some(text) = open.text {
            crate::scrollback::persist_block(&self.db, self.instance_id, &open.kind, &text, false)
                .await?;
        }
        Ok(())
    }

    /// Clean close for every remaining open block. Called when the
    /// workflow body exits and we're about to send `Done` — leftover
    /// opens get a real `BlockStop` so the TUI's per-id active state
    /// drains. Blocks that never received any text are dropped without
    /// being persisted (the client never materialised them either).
    async fn close_all_stop(&mut self, events: &EventsChannel) -> Result<()> {
        let drained: Vec<OpenBlock> = self.open.drain().map(|(_, v)| v).collect();
        for open in drained {
            events.send(StreamFrame::BlockStop { id: open.id });
            if let Some(text) = open.text {
                crate::scrollback::persist_block(
                    &self.db,
                    self.instance_id,
                    &open.kind,
                    &text,
                    false,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Dehydrate close-all: the workflow is going away while blocks are
    /// in flight. Persist each row marked truncated and drop the
    /// entries. No wire frames are emitted — the TUI is about to be
    /// told to clear and replay via `ScrollbackReset`, and the replay
    /// will surface these rows as `BlockTruncated`. Unmaterialised
    /// blocks (never wrote anything) are dropped silently.
    async fn close_all_truncate(&mut self) -> Result<()> {
        let drained: Vec<OpenBlock> = self.open.drain().map(|(_, v)| v).collect();
        for open in drained {
            if let Some(text) = open.text {
                crate::scrollback::persist_block(
                    &self.db,
                    self.instance_id,
                    &open.kind,
                    &text,
                    true,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Persist an error row alongside the in-flight stream's `Error`
    /// frame so it survives daemon restarts and workflow switches.
    async fn persist_error(&self, text: &str) -> Result<()> {
        crate::scrollback::persist_error(&self.db, self.instance_id, text).await?;
        Ok(())
    }
}

/// Top-level entry from the prompt RPC. Parses the input and either
/// pushes a fresh JS workflow (for slash commands) or hands the text
/// to the topmost workflow. Always finishes one "cycle" — i.e. drives
/// the topmost workflow until it parks waiting for input or
/// terminates.
pub(crate) async fn cycle(
    runtime: &Arc<SessionRuntime>,
    text: &str,
) -> Result<()> {
    match parse_slash_command(text) {
        Ok(Some((name, args))) => push_and_drive(runtime, name, args).await,
        Ok(None) => dispatch_topmost(runtime, text).await,
        Err(error) => {
            runtime
                .events
                .send(StreamFrame::Error(format!("bad workflow args: {error}")));
            Ok(())
        }
    }
}

/// Boot-time entry. Either restores the persisted stack (hydrating
/// the row with `active = 1`) or, if the table is literally empty,
/// seats the configured `default_workflow` via the normal push path
/// — which inserts its row and hydrates it in one shot.
///
/// Errors during hydration (missing config, migration drift, runtime
/// error) cascade through [`drop_active_and_promote`] until a row
/// hydrates cleanly or the live stack is exhausted. The daemon is
/// always usable when this returns.
pub(crate) async fn restore_or_seed(runtime: &Arc<SessionRuntime>) -> Result<()> {
    let db = &runtime.workflow_stack.db;

    if row_count(db).await? == 0 {
        let default_workflow = runtime.default_workflow.get();
        let Some(name) = default_workflow.as_deref().and_then(|opt| opt.as_deref()) else {
            return Ok(());
        };
        match push_default_workflow(runtime, name).await {
            Ok(()) => {}
            Err(error) => warn!(%error, workflow = %name, "default_workflow start failed"),
        }
        return Ok(());
    }

    hydrate_active_or_cascade(runtime).await
}

/// Push the configured default workflow with empty args. Used by
/// `restore_or_seed` when the table is empty (first-ever boot or a
/// fresh session). Frames the workflow emits during top-level
/// evaluation buffer in `WorkflowHandle::frames` and flush on the
/// first prompt cycle — there is no stream to write to here.
async fn push_default_workflow(runtime: &Arc<SessionRuntime>, name: &str) -> Result<()> {
    let workflows = runtime.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        warn!(
            workflow = name,
            "default_workflow is set but no matching [workflows.*] entry exists; \
             leaving stack empty"
        );
        return Ok(());
    };
    let migrations = match load_migrations(cfg).await {
        Ok(m) => m,
        Err(error) => {
            warn!(
                workflow = name,
                %error,
                "default_workflow migration read failed; leaving stack empty"
            );
            return Ok(());
        }
    };
    let instance_id = Uuid::new_v4();
    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args: Vec::new(),
        entity: cfg.id,
        instance_id,
        migrations,
    };
    let handle = runtime.workflow_runtime.start(invocation).await?;
    insert_pushed_row(&runtime.workflow_stack.db, name, instance_id, &[]).await?;
    *runtime.workflow_stack.top.lock().await = Some(WorkflowInstance {
        handle,
        emit: EmitState::new(runtime.workflow_stack.db.clone(), instance_id),
        config_key: name.to_owned(),
    });
    Ok(())
}

/// Slash-command push path: dehydrate the current top (if any), start
/// the new workflow, install it as the top, drive its initial cycle.
/// If the initial cycle exits, fall through to the pop+rehydrate path
/// so the caller never ends up "with no top" purely because the new
/// workflow returned synchronously.
async fn push_and_drive(
    runtime: &Arc<SessionRuntime>,
    name: &str,
    args: Vec<String>,
) -> Result<()> {
    let workflows = runtime.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        runtime
            .events
            .send(StreamFrame::Error(format!("unknown workflow: {name}")));
        return Ok(());
    };

    let migrations = match load_migrations(cfg).await {
        Ok(m) => m,
        Err(error) => {
            runtime.events.send(StreamFrame::Error(format!("workflow: {error}")));
            return Ok(());
        }
    };

    let instance_id = Uuid::new_v4();
    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args: args.clone(),
        entity: cfg.id,
        instance_id,
        migrations,
    };

    // Start the new runtime BEFORE we touch any state. If it fails,
    // the previous top (if any) keeps running and the DB is unchanged.
    let handle = match runtime.workflow_runtime.start(invocation).await {
        Ok(handle) => handle,
        Err(error) => {
            runtime.events.send(StreamFrame::Error(format!("workflow: {error}")));
            return Ok(());
        }
    };

    // Take the old top out of memory (we'll dehydrate it next). The
    // dehydration is bounded by `DEHYDRATE_TIMEOUT`; the JS body's
    // `lifecycle.shutdown` hook (if any) runs first.
    let old = runtime.workflow_stack.top.lock().await.take();
    if let Some(old) = old {
        dehydrate(runtime, old).await?;
    }

    // Persist the new row. Truncates any non-completed rows above the
    // (now demoted) current top — defensive against crash-mid-pop.
    insert_pushed_row(&runtime.workflow_stack.db, name, instance_id, &args).await?;

    // Tell the TUI to drop the previous workflow's in-memory scrollback
    // and replay the new active instance's (empty on a fresh push, but
    // we run the burst anyway for protocol uniformity — and for the
    // future "resume previously-popped" case which would have rows).
    crate::scrollback::replay_to_channel(&runtime.events, &runtime.workflow_stack.db, instance_id).await?;

    let mut new_instance = WorkflowInstance {
        handle,
        emit: EmitState::new(runtime.workflow_stack.db.clone(), instance_id),
        config_key: name.to_owned(),
    };
    let exited = drive(runtime, &mut new_instance).await?;
    if exited {
        // The new workflow ran to completion in its initial cycle.
        // Treat that as an immediate pop: tombstone its row, then
        // rehydrate whatever's underneath.
        drop_active_and_promote(runtime, instance_id).await?;
    } else {
        *runtime.workflow_stack.top.lock().await = Some(new_instance);
    }
    Ok(())
}

/// Hand `text` to the topmost workflow's inbox and drive a cycle. On
/// exit, tombstone the row and rehydrate the next live row (if any).
async fn dispatch_topmost(
    runtime: &Arc<SessionRuntime>,
    text: &str,
) -> Result<()> {
    let mut top = match runtime.workflow_stack.top.lock().await.take() {
        Some(top) => top,
        None => {
            runtime.events.send(StreamFrame::Error(
                "no workflow is active; use a slash command or set \
                 `default_workflow` in your config"
                    .to_owned(),
            ));
            return Ok(());
        }
    };

    // Sending to a dropped receiver would mean the body has already
    // exited and we just didn't observe it yet; treat that as
    // "exited" and let the drive loop confirm.
    let _ = top.handle.input_tx.send(UserInput {
        content: text.to_owned(),
    });
    let exited = drive(runtime, &mut top).await?;
    if exited {
        let instance_id = top.handle.instance;
        // Drop the in-memory state explicitly so its task is gone
        // before we begin rehydrating the next row.
        drop(top);
        drop_active_and_promote(runtime, instance_id).await?;
    } else {
        *runtime.workflow_stack.top.lock().await = Some(top);
    }
    Ok(())
}

/// Read each declared migration file (resolved relative to the
/// workflow's `cfg.file` directory) into memory.
async fn load_migrations(cfg: &WorkflowConfig) -> Result<Vec<Migration>, WorkflowError> {
    let base = cfg
        .file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(cfg.migrations.len());
    for path in &cfg.migrations {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            base.join(path)
        };
        let sql = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|source| WorkflowError::ReadMigration {
                path: resolved.clone(),
                source,
            })?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        out.push(Migration {
            name: std::borrow::Cow::Owned(name),
            sql: std::borrow::Cow::Owned(sql),
        });
    }
    Ok(out)
}

/// Request graceful shutdown on `instance`, drain its frames (so any
/// final messages from `lifecycle.shutdown` reach the client), and
/// wait for the body to exit. Bounded by [`DEHYDRATE_TIMEOUT`].
///
/// Dropping `instance` at the end aborts the spawned task as a final
/// fallback — but in practice the body has already exited by the time
/// we get here unless the timeout fired.
async fn dehydrate(
    runtime: &Arc<SessionRuntime>,
    mut instance: WorkflowInstance,
) -> Result<()> {
    instance.handle.request_shutdown();
    let deadline = tokio::time::sleep(DEHYDRATE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        // Flush anything sitting in the queue first.
        while let Ok(host_frame) = instance.handle.frames.try_recv() {
            emit(runtime, &mut instance.emit, host_frame).await?;
        }
        tokio::select! {
            biased;
            Some(host_frame) = instance.handle.frames.recv() => {
                emit(runtime, &mut instance.emit, host_frame).await?;
            }
            done = &mut instance.handle.done => {
                // Drain any tail frames the lifecycle hook pushed
                // immediately before settling.
                while let Ok(host_frame) = instance.handle.frames.try_recv() {
                    emit(runtime, &mut instance.emit, host_frame).await?;
                }
                // Body exited cleanly: every remaining open block gets
                // a clean BlockStop on the wire and a non-truncated row.
                instance.emit.close_all_stop(&runtime.events).await?;
                if let Ok(Err(error)) = done {
                    let msg = format!("workflow shutdown: {error}");
                    instance.emit.persist_error(&msg).await?;
                    runtime.events.send(StreamFrame::Error(msg));
                }
                return Ok(());
            }
            () = &mut deadline => {
                warn!(
                    instance = %instance.handle.instance,
                    "workflow shutdown timed out; force-dropping handle"
                );
                // Body never settled. Mark every in-flight block
                // truncated. No wire BlockStop — the TUI is about to
                // be told to ScrollbackReset by the caller's push path.
                instance.emit.close_all_truncate().await?;
                return Ok(());
            }
        }
    }
}

/// Drains the instance's host-frame channel until the body either
/// parks waiting for input or terminates. Returns `true` if the body
/// exited.
async fn drive(
    runtime: &Arc<SessionRuntime>,
    instance: &mut WorkflowInstance,
) -> Result<bool> {
    loop {
        while let Ok(host_frame) = instance.handle.frames.try_recv() {
            emit(runtime, &mut instance.emit, host_frame).await?;
        }
        tokio::select! {
            biased;
            Some(host_frame) = instance.handle.frames.recv() => {
                emit(runtime, &mut instance.emit, host_frame).await?;
            }
            done = &mut instance.handle.done => {
                while let Ok(host_frame) = instance.handle.frames.try_recv() {
                    emit(runtime, &mut instance.emit, host_frame).await?;
                }
                instance.emit.close_all_stop(&runtime.events).await?;
                if let Ok(Err(error)) = done {
                    let msg = format!("workflow: {error}");
                    instance.emit.persist_error(&msg).await?;
                    runtime.events.send(StreamFrame::Error(msg));
                } else if let Err(error) = done {
                    warn!(%error, "workflow done channel closed without value");
                }
                return Ok(true);
            }
            () = instance.handle.parked.notified() => {
                while let Ok(host_frame) = instance.handle.frames.try_recv() {
                    emit(runtime, &mut instance.emit, host_frame).await?;
                }
                return Ok(false);
            }
        }
    }
}

async fn emit(
    runtime: &Arc<SessionRuntime>,
    state: &mut EmitState,
    frame: HostFrame,
) -> Result<()> {
    match frame {
        HostFrame::Push(FramePush { id: frame_id, kind }) => match kind {
            FrameKind::Markdown { content, sender } => {
                let block_kind = BlockKind::Text {
                    sender: sender.map(Arc::from),
                };
                let block = state.alloc();
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: block_kind.clone(),
                    text: content.clone(),
                });
                state.open.insert(
                    frame_id,
                    OpenBlock {
                        id: block,
                        kind: block_kind,
                        text: content,
                    },
                );
            }
            FrameKind::ShellOutput {
                state: shell_state,
                cmd,
                content,
            } => {
                let block_kind = BlockKind::ShellOutput {
                    state: shell_state_to_protocol(&shell_state),
                    cmd: Arc::from(cmd),
                };
                let block = state.alloc();
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: block_kind.clone(),
                    text: Some(content.clone()),
                });
                state.open.insert(
                    frame_id,
                    OpenBlock {
                        id: block,
                        kind: block_kind,
                        text: Some(content),
                    },
                );
            }
            FrameKind::Error { content } => {
                state.persist_error(&content).await?;
                runtime.events.send(StreamFrame::Error(content));
            }
            FrameKind::ToolUse { name, detail } => {
                let block = state.alloc();
                let name_arc: Arc<str> = Arc::from(name);
                let detail_arc: Option<Arc<str>> = detail.map(Arc::from);
                let kind = BlockKind::ToolUse {
                    name: name_arc.clone(),
                    detail: detail_arc,
                };
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: kind.clone(),
                    text: Some(String::new()),
                });
                // One-shot: stop + persist immediately, no entry in
                // `state.open`. Text is empty — the name lives in the
                // prefix on the TUI side.
                runtime.events.send(StreamFrame::BlockStop { id: block });
                crate::scrollback::persist_block(&state.db, state.instance_id, &kind, "", false)
                    .await?;
            }
            FrameKind::Json { tag, value } => {
                let body =
                    serde_json::to_string(&value).unwrap_or_else(|_| "<unserializable>".into());
                let block = state.alloc();
                let kind = BlockKind::Text { sender: None };
                let text = format!("[{tag}] {body}");
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: kind.clone(),
                    text: Some(text.clone()),
                });
                // Open + persist + close in one go: a JsonFrame is a
                // one-shot block. It never enters `state.open`.
                runtime.events.send(StreamFrame::BlockStop { id: block });
                crate::scrollback::persist_block(&state.db, state.instance_id, &kind, &text, false)
                    .await?;
            }
            FrameKind::Diff { lines } => {
                let wire_lines: Vec<crate::events::DiffLine> =
                    lines.into_iter().map(diff_op_to_protocol).collect();
                let block = state.alloc();
                let kind = BlockKind::Diff { lines: wire_lines };
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: kind.clone(),
                    text: Some(String::new()),
                });
                // One-shot like ToolUse / Json — Push + Stop in the
                // same batch, never enters `state.open`.
                runtime.events.send(StreamFrame::BlockStop { id: block });
                crate::scrollback::persist_block(&state.db, state.instance_id, &kind, "", false)
                    .await?;
            }
        },
        HostFrame::Append {
            id: frame_id,
            delta,
        } => {
            if let Some(open) = state.open.get_mut(&frame_id) {
                match &mut open.text {
                    Some(buf) => buf.push_str(&delta),
                    slot @ None => *slot = Some(delta.clone()),
                }
                let block = open.id;
                let kind = open.kind.clone();
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind,
                    text: Some(delta),
                });
            }
        }
        HostFrame::UpdateKind { id: frame_id, kind } => {
            // Translate the workflow's FrameKind delta into a wire
            // BlockKind delta. Frame kinds without a streaming
            // representation on the wire (Error, Json) are no-ops.
            let new_block_kind = match kind {
                FrameKind::Markdown { sender, .. } => Some(BlockKind::Text {
                    sender: sender.map(Arc::from),
                }),
                FrameKind::ShellOutput {
                    state: shell_state,
                    cmd,
                    ..
                } => Some(BlockKind::ShellOutput {
                    state: shell_state_to_protocol(&shell_state),
                    cmd: Arc::from(cmd),
                }),
                FrameKind::Error { .. }
                | FrameKind::Json { .. }
                | FrameKind::ToolUse { .. }
                | FrameKind::Diff { .. } => None,
            };
            let Some(new_block_kind) = new_block_kind else {
                return Ok(());
            };
            if let Some(open) = state.open.get_mut(&frame_id) {
                open.kind = new_block_kind.clone();
                let block = open.id;
                // No text on a kind-only delta. The client either
                // updates the kind on a materialised block (re-render)
                // or, if the block was never materialised, just stores
                // the new kind for whenever the first body delta lands.
                runtime.events.send(StreamFrame::BlockDelta {
                    id: block,
                    kind: new_block_kind,
                    text: None,
                });
            }
        }
        HostFrame::Close { id: frame_id } => {
            state.close_one(&runtime.events, frame_id).await?;
        }
        HostFrame::Usage(usage) => {
            runtime.events.send(StreamFrame::Usage(usage));
        }
        HostFrame::Permission {
            request,
            allow_auto,
        } => {
            if allow_auto {
                let id = request.id;
                let outcome = crate::runtime::auto_judge::judge(runtime, &request).await;
                match outcome {
                    crate::runtime::auto_judge::JudgeOutcome::Approve { reason } => {
                        if let Err(error) = runtime.permissions.respond(
                            id,
                            frances_workflow::PermissionResponse::Yes {
                                details: Some(reason),
                            },
                        ) {
                            warn!(%error, %id, "auto-judge approve: respond failed");
                        }
                    }
                    crate::runtime::auto_judge::JudgeOutcome::Reject { reason }
                    | crate::runtime::auto_judge::JudgeOutcome::Indeterminate { reason } => {
                        tracing::debug!(%id, %reason, "auto-judge fell through to user");
                        runtime.events.send(StreamFrame::Permission(request));
                    }
                }
            } else {
                runtime.events.send(StreamFrame::Permission(request));
            }
        }
    }
    Ok(())
}

fn diff_op_to_protocol(op: frances_edit::DiffOp) -> crate::events::DiffLine {
    use frances_edit::DiffOp;
    match op {
        DiffOp::Context { text, line } => crate::events::DiffLine::Context {
            text: Arc::from(text),
            line,
        },
        DiffOp::Added(t) => crate::events::DiffLine::Added(Arc::from(t)),
        DiffOp::Removed(t) => crate::events::DiffLine::Removed(Arc::from(t)),
    }
}

fn shell_state_to_protocol(state: &frances_workflow::ShellState) -> crate::events::ShellState {
    use frances_workflow::ShellState as W;
    match state {
        W::Running => crate::events::ShellState::Running,
        W::Success => crate::events::ShellState::Success,
        W::Exit(n) => crate::events::ShellState::Exit(*n),
    }
}

// --- Persistence helpers --------------------------------------------------

/// SQL helper: tombstone the row matching `instance_id` and promote
/// the next live row to `active = 1`. Then hydrate the new top in
/// memory (if any). If hydration fails, recurse — tombstoning the
/// failed row's branch — until either a row hydrates cleanly or the
/// live stack is exhausted (top stays `None`).
async fn drop_active_and_promote(
    runtime: &Arc<SessionRuntime>,
    instance_id: Uuid,
) -> Result<()> {
    mark_completed_and_promote(&runtime.workflow_stack.db, instance_id).await?;
    hydrate_active_or_cascade(runtime).await?;
    // Tell the TUI to clear scrollback and replay the newly-promoted
    // workflow's history (if any row was promoted). When the stack ran
    // dry there's no instance to replay — we still emit an empty reset
    // so the previous workflow's in-memory scrollback is dropped.
    let new_top_instance = runtime
        .workflow_stack
        .top
        .lock()
        .await
        .as_ref()
        .map(|i| i.handle.instance);
    if let Some(new_instance) = new_top_instance {
        crate::scrollback::replay_to_channel(
            &runtime.events,
            &runtime.workflow_stack.db,
            new_instance,
        )
        .await?;
    } else {
        runtime.events.send(StreamFrame::ScrollbackReset {
            instance_id: Uuid::nil(),
        });
        runtime.events.send(StreamFrame::ScrollbackReplayEnd);
    }
    Ok(())
}

/// Find the row with `active = 1` and hydrate it as the in-memory top.
/// On any failure, tombstone the row + everything at or above its
/// position and promote the next live row; retry. Loops until the
/// stack hydrates or runs dry.
async fn hydrate_active_or_cascade(runtime: &Arc<SessionRuntime>) -> Result<()> {
    let db = &runtime.workflow_stack.db;
    loop {
        let Some(row) = read_active_row(db).await? else {
            *runtime.workflow_stack.top.lock().await = None;
            return Ok(());
        };

        match hydrate(runtime, &row).await {
            Ok(instance) => {
                *runtime.workflow_stack.top.lock().await = Some(instance);
                return Ok(());
            }
            Err(error) => {
                warn!(
                    instance = %row.instance_id,
                    config = %row.config_key,
                    %error,
                    "workflow restore failed; tombstoning and trying next"
                );
                truncate_at_or_above(db, row.position).await?;
                // Loop: try to promote next non-completed.
            }
        }
    }
}

/// Attempt to hydrate a single row: look up its config, load
/// migrations, start the runtime with the row's `instance_id`
/// preserved.
async fn hydrate(
    runtime: &Arc<SessionRuntime>,
    row: &StackRow,
) -> Result<WorkflowInstance, WorkflowError> {
    let workflows = runtime.workflows.get_or_default();
    let cfg = workflows
        .get(&row.config_key)
        .ok_or_else(|| WorkflowError::ScriptCaught {
            context: "restore".into(),
            detail: format!("no [workflows.{}] entry in config", row.config_key),
        })?;
    let migrations = load_migrations(cfg).await?;
    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args: row.args.clone(),
        entity: cfg.id,
        instance_id: row.instance_id,
        migrations,
    };
    let handle = runtime.workflow_runtime.start(invocation).await?;
    Ok(WorkflowInstance {
        handle,
        emit: EmitState::new(runtime.workflow_stack.db.clone(), row.instance_id),
        config_key: row.config_key.clone(),
    })
}

/// Decoded `workflow_stack` row.
struct StackRow {
    position: i64,
    config_key: String,
    instance_id: Uuid,
    args: Vec<String>,
}

/// Push transaction: truncate any non-completed rows above the
/// current top (defensive), demote the current top, insert the new
/// active row.
async fn insert_pushed_row(
    db: &Database,
    config_key: &str,
    instance_id: Uuid,
    args: &[String],
) -> Result<(), WorkflowStackError> {
    let now = now_ns();
    let args_json = serde_json::to_string(args)?;
    let instance_bytes = instance_id.as_bytes().to_vec();

    let conn = db.connect().await;
    let tx = conn.unchecked_transaction().await?;
    tx.execute(
        "UPDATE workflow_stack
            SET completed_at = ?1
          WHERE completed_at IS NULL
            AND position > COALESCE(
              (SELECT MAX(position) FROM workflow_stack WHERE active = 1),
              -1
            )",
        (now,),
    )
    .await?;
    tx.execute("UPDATE workflow_stack SET active = 0 WHERE active = 1", ())
        .await?;
    tx.execute(
        "INSERT INTO workflow_stack
             (config_key, instance_id, args, created_at, active)
         VALUES (?1, ?2, ?3, ?4, 1)",
        (config_key.to_string(), instance_bytes, args_json, now),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Pop transaction: tombstone the active row (or, if no active row
/// matches the given `instance_id`, tombstone by `instance_id`) and
/// promote the next live row to active.
async fn mark_completed_and_promote(
    db: &Database,
    instance_id: Uuid,
) -> Result<(), WorkflowStackError> {
    let now = now_ns();
    let instance_bytes = instance_id.as_bytes().to_vec();

    let conn = db.connect().await;
    let tx = conn.unchecked_transaction().await?;
    tx.execute(
        "UPDATE workflow_stack
            SET active = 0, completed_at = ?1
          WHERE instance_id = ?2",
        (now, instance_bytes),
    )
    .await?;
    tx.execute(
        "UPDATE workflow_stack
            SET active = 1
          WHERE position = (
            SELECT MAX(position)
              FROM workflow_stack
             WHERE completed_at IS NULL
          )",
        (),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Cascade-tombstone helper used on hydrate failure: mark this row
/// and everything above it as completed, then promote the next live
/// row.
async fn truncate_at_or_above(db: &Database, position: i64) -> Result<(), WorkflowStackError> {
    let now = now_ns();
    let conn = db.connect().await;
    let tx = conn.unchecked_transaction().await?;
    tx.execute(
        "UPDATE workflow_stack
            SET active = 0, completed_at = ?1
          WHERE completed_at IS NULL
            AND position >= ?2",
        (now, position),
    )
    .await?;
    tx.execute(
        "UPDATE workflow_stack
            SET active = 1
          WHERE position = (
            SELECT MAX(position)
              FROM workflow_stack
             WHERE completed_at IS NULL
          )",
        (),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn row_count(db: &Database) -> Result<i64, WorkflowStackError> {
    let conn = db.connect().await;
    let mut rows = conn
        .query("SELECT COUNT(*) FROM workflow_stack", ())
        .await?;
    let row = rows.next().await?.expect("COUNT(*) always returns one row");
    match row.get_value(0)? {
        Value::Integer(n) => Ok(n),
        other => Err(WorkflowStackError::UnexpectedColumn {
            column: "COUNT(*)",
            expected: "integer",
            found: other,
        }),
    }
}

async fn read_active_row(db: &Database) -> Result<Option<StackRow>, WorkflowStackError> {
    let conn = db.connect().await;
    let mut rows = conn
        .query(
            "SELECT position, config_key, instance_id, args
               FROM workflow_stack
              WHERE active = 1
              LIMIT 1",
            (),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };

    let position = match row.get_value(0)? {
        Value::Integer(v) => v,
        other => {
            return Err(WorkflowStackError::UnexpectedColumn {
                column: "position",
                expected: "integer",
                found: other,
            });
        }
    };
    let config_key = match row.get_value(1)? {
        Value::Text(t) => t,
        other => {
            return Err(WorkflowStackError::UnexpectedColumn {
                column: "config_key",
                expected: "text",
                found: other,
            });
        }
    };
    let instance_bytes = match row.get_value(2)? {
        Value::Blob(b) => b,
        other => {
            return Err(WorkflowStackError::UnexpectedColumn {
                column: "instance_id",
                expected: "blob",
                found: other,
            });
        }
    };
    if instance_bytes.len() != 16 {
        return Err(WorkflowStackError::InstanceIdLength {
            got: instance_bytes.len(),
        });
    }
    let instance_id = Uuid::from_slice(&instance_bytes).expect("checked length");
    let args_text = match row.get_value(3)? {
        Value::Text(t) => t,
        other => {
            return Err(WorkflowStackError::UnexpectedColumn {
                column: "args",
                expected: "text",
                found: other,
            });
        }
    };
    let args: Vec<String> = serde_json::from_str(&args_text)?;
    Ok(Some(StackRow {
        position,
        config_key,
        instance_id,
        args,
    }))
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the workflow_stack persistence helpers.
    //!
    //! These exercise the SQL layer in isolation against a fresh
    //! in-memory turso connection — no runtime, no `ServerState`. The
    //! end-to-end hydrate/dehydrate path is covered by the workflow
    //! runtime's own test suite plus exercises the daemon's other
    //! integration tests indirectly.
    use super::*;
    use frances_storage::run_all;

    async fn fresh_db() -> Database {
        let db = Database::open_in_memory().await.unwrap();
        {
            let conn = db.connect().await;
            run_all(&conn, &[&SCHEMA]).await.unwrap();
        }
        db
    }

    /// Count live (non-completed) rows.
    async fn count_live(db: &Database) -> i64 {
        let conn = db.connect().await;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM workflow_stack WHERE completed_at IS NULL",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        match row.get_value(0).unwrap() {
            Value::Integer(n) => n,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Read `(active, completed_at IS NULL)` flags for the given
    /// `instance_id`. Returns `None` if the row does not exist.
    async fn flags_for(db: &Database, instance_id: Uuid) -> Option<(bool, bool)> {
        let conn = db.connect().await;
        let mut rows = conn
            .query(
                "SELECT active, completed_at FROM workflow_stack WHERE instance_id = ?1",
                (instance_id.as_bytes().to_vec(),),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap()?;
        let active = matches!(row.get_value(0).unwrap(), Value::Integer(1));
        let alive = matches!(row.get_value(1).unwrap(), Value::Null);
        Some((active, alive))
    }

    #[tokio::test]
    async fn fresh_table_is_empty() {
        let db = fresh_db().await;
        assert_eq!(row_count(&db).await.unwrap(), 0);
        assert!(read_active_row(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn single_push_records_and_reads_back() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        insert_pushed_row(&db, "main", id, &["arg1".into(), "arg2".into()])
            .await
            .unwrap();

        assert_eq!(row_count(&db).await.unwrap(), 1);
        let row = read_active_row(&db).await.unwrap().expect("active row");
        assert_eq!(row.config_key, "main");
        assert_eq!(row.instance_id, id);
        assert_eq!(row.args, vec!["arg1".to_owned(), "arg2".to_owned()]);
    }

    #[tokio::test]
    async fn second_push_demotes_first_and_takes_top() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();
        insert_pushed_row(&db, "b", b, &[]).await.unwrap();

        // Both rows alive; B is on top.
        assert_eq!(count_live(&db).await, 2);
        let active = read_active_row(&db).await.unwrap().expect("top");
        assert_eq!(active.instance_id, b);
        assert_eq!(flags_for(&db, a).await, Some((false, true)));
        assert_eq!(flags_for(&db, b).await, Some((true, true)));
    }

    #[tokio::test]
    async fn pop_tombstones_and_promotes_previous() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();
        insert_pushed_row(&db, "b", b, &[]).await.unwrap();

        mark_completed_and_promote(&db, b).await.unwrap();

        // B is dead, A is back on top.
        assert_eq!(flags_for(&db, b).await, Some((false, false)));
        assert_eq!(flags_for(&db, a).await, Some((true, true)));
        assert_eq!(
            read_active_row(&db)
                .await
                .unwrap()
                .expect("top")
                .instance_id,
            a
        );
    }

    #[tokio::test]
    async fn pop_to_empty_stack_leaves_no_active_row() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();

        mark_completed_and_promote(&db, a).await.unwrap();

        assert_eq!(flags_for(&db, a).await, Some((false, false)));
        assert!(read_active_row(&db).await.unwrap().is_none());
        // The row is still in the table — append-only.
        assert_eq!(row_count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn push_truncates_orphans_above_current_top() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();
        insert_pushed_row(&db, "b", b, &[]).await.unwrap();

        // Forcibly clear `active` from B without tombstoning it. This
        // simulates a crash where the daemon went down mid-pop after
        // clearing active but before setting completed_at.
        {
            let conn = db.connect().await;
            conn.execute("UPDATE workflow_stack SET active = 0", ())
                .await
                .unwrap();
            // Now A is the highest position with completed_at NULL,
            // but B sits above it. A push (orphan-truncation step)
            // should tombstone B before inserting C. First make A the
            // active top again so the truncation rule picks B
            // (position > A) as the orphan.
            conn.execute(
                "UPDATE workflow_stack SET active = 1 WHERE instance_id = ?1",
                (a.as_bytes().to_vec(),),
            )
            .await
            .unwrap();
        }

        insert_pushed_row(&db, "c", c, &[]).await.unwrap();

        // B is now tombstoned (truncated); A is demoted; C is active.
        assert_eq!(flags_for(&db, b).await, Some((false, false)));
        assert_eq!(flags_for(&db, a).await, Some((false, true)));
        assert_eq!(flags_for(&db, c).await, Some((true, true)));
    }

    #[tokio::test]
    async fn pop_then_push_walks_back_via_truncation() {
        // A push above C, then user pops C: B is the active top, C is
        // still alive (resumeable in principle). User pushes D: the
        // C row gets tombstoned (orphan above B). D ends up on top.
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let d = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();
        insert_pushed_row(&db, "b", b, &[]).await.unwrap();
        insert_pushed_row(&db, "c", c, &[]).await.unwrap();

        // Pop C — but with the "pop doesn't tombstone the previous
        // active" semantics: in our current design we DO tombstone,
        // so let's just confirm the truncation rule via the explicit
        // crash-style setup of the previous test rather than this
        // narrative. Here we follow the implemented contract: C is
        // tombstoned, B is promoted.
        mark_completed_and_promote(&db, c).await.unwrap();
        assert_eq!(flags_for(&db, c).await, Some((false, false)));
        assert_eq!(flags_for(&db, b).await, Some((true, true)));

        insert_pushed_row(&db, "d", d, &[]).await.unwrap();
        // C stays tombstoned, B demoted, D on top, A demoted.
        assert_eq!(flags_for(&db, a).await, Some((false, true)));
        assert_eq!(flags_for(&db, b).await, Some((false, true)));
        assert_eq!(flags_for(&db, c).await, Some((false, false)));
        assert_eq!(flags_for(&db, d).await, Some((true, true)));
    }

    #[tokio::test]
    async fn truncate_at_or_above_kills_row_and_everything_higher() {
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();
        insert_pushed_row(&db, "b", b, &[]).await.unwrap();
        insert_pushed_row(&db, "c", c, &[]).await.unwrap();

        // Find B's position.
        let b_pos = {
            let conn = db.connect().await;
            let mut rows = conn
                .query(
                    "SELECT position FROM workflow_stack WHERE instance_id = ?1",
                    (b.as_bytes().to_vec(),),
                )
                .await
                .unwrap();
            match rows.next().await.unwrap().unwrap().get_value(0).unwrap() {
                Value::Integer(n) => n,
                other => panic!("unexpected {other:?}"),
            }
        };

        truncate_at_or_above(&db, b_pos).await.unwrap();

        // A survives, B and C are tombstoned. A is now the active top.
        assert_eq!(flags_for(&db, a).await, Some((true, true)));
        assert_eq!(flags_for(&db, b).await, Some((false, false)));
        assert_eq!(flags_for(&db, c).await, Some((false, false)));
    }

    #[tokio::test]
    async fn args_with_special_chars_round_trip() {
        let db = fresh_db().await;
        let id = Uuid::new_v4();
        let args: Vec<String> = vec![
            "plain".into(),
            "with \"quotes\"".into(),
            "tab\there".into(),
            String::new(),
        ];
        insert_pushed_row(&db, "k", id, &args).await.unwrap();
        let row = read_active_row(&db).await.unwrap().expect("active");
        assert_eq!(row.args, args);
    }

    #[tokio::test]
    async fn unique_active_constraint_holds() {
        // The partial unique index forbids two active=1 rows at once.
        // `insert_pushed_row` demotes the previous active before
        // inserting the new one, so this never triggers in normal
        // flow; verify the index is in place by attempting a manual
        // conflicting INSERT.
        let db = fresh_db().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_pushed_row(&db, "a", a, &[]).await.unwrap();

        // Manually insert a second active row, bypassing our helper.
        let err = {
            let conn = db.connect().await;
            conn.execute(
                "INSERT INTO workflow_stack
                 (config_key, instance_id, args, created_at, active)
                 VALUES (?1, ?2, ?3, 0, 1)",
                ("b".to_string(), b.as_bytes().to_vec(), "[]".to_string()),
            )
            .await
            .unwrap_err()
        };
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique") || msg.contains("constraint"),
            "expected unique constraint failure, got: {msg}"
        );
    }
}
