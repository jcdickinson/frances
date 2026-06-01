//! Per-session workflow stack.
//!
//! ## Single-slot in-memory, multi-level in DB
//!
//! At any time only one workflow runs — the **top** of the stack, owned
//! by the long-lived driver task (`run_driver`). Levels below the top
//! live as rows in the `workflow_stack` table (`active = 0`,
//! `completed_at IS NULL`). When
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
//! with the next AUTOINCREMENT position. Rows accumulate.
//!
//! ## Boot
//!
//! `restore_or_seed` reads the table. If `COUNT(*) = 0`, it pushes
//! the configured `default_workflow` (which inserts the first row and
//! hydrates). Otherwise it hydrates the row with `active = 1`. If no
//! row is active (the user popped everything down to zero live rows
//! in a previous session), the stack starts empty — the default
//! workflow is **not** re-seeded.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use frances_core::{now_ns, resolve_relative};
use parking_lot::Mutex as PlMutex;
use thiserror::Error;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::warn;
use turso::Value;
use uuid::Uuid;

use crate::Result;
use crate::events::{ScrollbackFrame, StreamFrame};
use crate::runtime::{EventsChannel, SessionRuntime};
use crate::store::Database;

use frances_storage::{EntitySchema, Migration};
use frances_workflow::{
    InboxItem, Invocation, PermissionRequest, SectionId, SectionKind, SectionSpec,
    SectionTranscript, SurfaceCmd, UserInput, WorkflowHandle, parse_slash_command,
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

/// The session-scoped workflow stack. The currently-hydrated workflow is
/// owned by the long-lived driver task (see `run_driver`), not held
/// here — this struct keeps the per-session [`Database`] plus the
/// "live wires" the rest of the runtime uses to reach the active
/// workflow without going through the driver: a clone of its inbox
/// sender and its `instance_id`.
pub struct WorkflowStack {
    db: Database,
    /// Sender into the active workflow's `inbox`. Set when the driver
    /// seats an instance, cleared when none is active. `prompt` and
    /// `interrupt` push straight onto this — input is just IO, delivered
    /// any time, decoupled from any cycle.
    active_input: PlMutex<Option<UnboundedSender<InboxItem>>>,
    /// `instance_id` of the active workflow, for callers (e.g.
    /// scrollback replay) that need to know which workflow is live.
    active_instance_id: PlMutex<Option<Uuid>>,
}

impl WorkflowStack {
    /// Builds an empty in-memory stack bound to `db`. Layering across
    /// process restarts lives entirely in the per-session
    /// `workflow_stack` table on this database.
    pub fn new(db: Database) -> Self {
        Self {
            db,
            active_input: PlMutex::new(None),
            active_instance_id: PlMutex::new(None),
        }
    }

    /// Per-session [`Database`] handle. Same lock the rest of the runtime
    /// uses; cheap clone for callers (like scrollback replay) that want
    /// to issue SQL against it without going through the stack itself.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// `instance_id` of the currently-hydrated workflow, if any.
    /// Read-only helper for the startup scrollback replay path, which
    /// needs to know which workflow to replay for.
    pub async fn active_instance(&self) -> Option<Uuid> {
        *self.active_instance_id.lock()
    }

    /// Publish the active wires for the driver's initial instance,
    /// synchronously during `SessionRuntime::start` (before the driver
    /// task is scheduled) so the startup scrollback replay sees it right
    /// away. No-op when the stack boots empty.
    pub(crate) fn seat_initial(&self, instance: Option<&WorkflowInstance>) {
        match instance {
            Some(inst) => self.set_active(inst),
            None => self.clear_active(),
        }
    }

    /// Record the active workflow's live wires. Called by the driver
    /// (and once up-front in `start`) when an instance becomes active.
    fn set_active(&self, instance: &WorkflowInstance) {
        *self.active_input.lock() = Some(instance.handle.input_tx.clone());
        *self.active_instance_id.lock() = Some(instance.handle.instance);
    }

    /// Clear the active wires when no workflow is hydrated.
    fn clear_active(&self) {
        *self.active_input.lock() = None;
        *self.active_instance_id.lock() = None;
    }

    /// Deliver an inbox item to the active workflow. No-op (dropped)
    /// when nothing is hydrated — same best-effort semantics as sending
    /// to an exited body.
    fn deliver(&self, item: InboxItem) {
        if let Some(tx) = self.active_input.lock().as_ref() {
            let _ = tx.send(item);
        }
    }
}

/// A command for the long-lived workflow driver. Input and interrupts
/// do *not* go through here — they're delivered straight to the active
/// inbox via [`WorkflowStack::deliver`]. Only stack-lifecycle changes
/// (slash-command pushes) need the driver to act.
pub(crate) enum DriverCmd {
    /// Push a fresh workflow on top of the stack (dehydrating the
    /// current active one first).
    Push { name: String, args: Vec<String> },
}

/// The currently-hydrated workflow plus the emit-state needed across
/// multiple `drive()` invocations (block id allocator, currently-open
/// block). The runtime `WorkflowHandle` already carries the `instance_id`.
pub(crate) struct WorkflowInstance {
    handle: WorkflowHandle,
    emit: EmitState,
}

/// Block-tracking state for a single hydrated workflow's lifetime.
///
/// Workflow frames map to event blocks like this:
///
/// - `MarkdownSection` push: open a new `Text { source }` block, write
///   initial content; the block stays open so subsequent `append`s
///   stream into it. The JS side sends `Close` for the prior active
///   markdown before pushing a new one — multiple blocks can be open
///   concurrently (e.g. a shell-output block running while the LLM
///   streams text into a markdown block above it).
/// - `MarkdownSection.append`: `Append` carries the [`SectionId`]; the
///   emit state looks up the matching open block and writes a
///   `BlockDelta` against it.
/// - `Close { id }`: emit `BlockStop` for the block, persist a clean
///   scrollback row, remove from `open`.
/// - `UpdateKind { id, kind }`: emit a no-text `BlockDelta` carrying
///   the new kind. Used for in-place metadata transitions (ShellOutput
///   Running → Success/Exit(N)).
/// - `ErrorSection` push: emit a one-shot `StreamFrame::Error` and
///   persist a scrollback row of kind 'error'. Does NOT touch any
///   open block — error frames are side-channel.
/// - `JsonSection` push: open + immediately close a one-shot
///   `Text { source: Source::Internal }` block rendering `[tag] body`.
///
/// On workflow termination every remaining open block is closed so the
/// UI's `BlockState` ends up Idle. `EmitState` accumulates the delta
/// text for each open block so we can persist the full body on close
/// — either clean (a `BlockStop`) or truncated (workflow was dehydrated
/// while a block was in flight).
struct EmitState {
    /// Open sections keyed by the workflow-side [`SectionId`]. Emit is
    /// single-threaded (one task per workflow instance) so a plain
    /// `HashMap` is enough.
    open: HashMap<SectionId, OpenSection>,
    /// Shared per-session [`Database`] used for scrollback writes. Cheap
    /// clone; the underlying connection lock serialises overlapping
    /// writes.
    db: Database,
    /// Identifies the workflow whose sections we're emitting. Every
    /// scrollback row written from this state is tagged with it so
    /// replay can scope by workflow.
    instance_id: Uuid,
}

/// A section whose first `SectionAppend` has been emitted but whose
/// `SectionClose` has not. We buffer the delta text here so that on
/// close we can write one scrollback row with the full body.
///
/// `text` accumulates from successive Appends; an Append with empty
/// `delta` is a metadata-only update (kind changed, e.g. shell state
/// `Running` → `Success`). On `Close` we persist the accumulated text
/// against the most recent kind.
struct OpenSection {
    kind: SectionKind,
    text: String,
    /// `true` when the section has received any non-empty body delta.
    /// Sections that only ever saw metadata-only updates aren't
    /// persisted on close — they're empty placeholders by construction.
    materialised: bool,
}

/// Why a set of open sections is being closed. The two variants carry
/// exactly what differs between a clean stop and a mid-stream dehydrate,
/// so the nonsense combinations (truncated-but-emit-close, or
/// clean-but-silent) can't be written.
enum CloseMode<'a> {
    /// The workflow body exited cleanly. Emit a `SectionClose` per
    /// section; persist non-truncated rows.
    Stop(&'a EventsChannel),
    /// The workflow is dehydrating mid-stream. Emit nothing; persist
    /// each row truncated.
    Truncate,
}

impl CloseMode<'_> {
    fn truncated(&self) -> bool {
        matches!(self, CloseMode::Truncate)
    }
}

impl EmitState {
    fn new(db: Database, instance_id: Uuid) -> Self {
        Self {
            open: HashMap::new(),
            db,
            instance_id,
        }
    }

    /// Persist one closing section's row (if it ever received body
    /// content) and, for a clean stop, emit its `SectionClose`. The
    /// `open` is consumed — its `kind`/`text` move straight into the
    /// row.
    async fn close_section(
        &self,
        mode: &CloseMode<'_>,
        id: SectionId,
        open: OpenSection,
    ) -> Result<()> {
        if let CloseMode::Stop(events) = mode {
            events.send(StreamFrame::SectionClose { id });
        }
        if open.materialised {
            crate::scrollback::persist_section(
                &self.db,
                self.instance_id,
                open.kind,
                open.text,
                mode.truncated(),
            )
            .await?;
        }
        Ok(())
    }

    /// Clean close for a single section: emit `SectionClose`, persist
    /// a finished row (if the section ever received body content),
    /// drop the entry. Idempotent on unknown ids.
    async fn close_one(&mut self, events: &EventsChannel, id: SectionId) -> Result<()> {
        let Some(open) = self.open.remove(&id) else {
            return Ok(());
        };
        self.close_section(&CloseMode::Stop(events), id, open).await
    }

    /// Close every remaining open section. `Stop` emits a `SectionClose`
    /// per section and persists clean rows; `Truncate` emits nothing
    /// (the TUI is about to clear and replay via `ScrollbackFrame::Reset`,
    /// which surfaces these rows as `SectionTruncated`) and marks each
    /// row truncated.
    async fn close_all(&mut self, mode: CloseMode<'_>) -> Result<()> {
        let drained: Vec<(SectionId, OpenSection)> = self.open.drain().collect();
        for (id, open) in drained {
            self.close_section(&mode, id, open).await?;
        }
        Ok(())
    }

    /// Persist an error row alongside the in-flight stream's `Error`
    /// frame so it survives process restarts and workflow switches.
    async fn persist_error(&self, text: &str) -> Result<()> {
        crate::scrollback::persist_error(&self.db, self.instance_id, text).await?;
        Ok(())
    }
}

/// Translate a prompt-RPC text into the right delivery. Slash commands
/// become a [`DriverCmd::Push`] (stack lifecycle, handled by the
/// driver); everything else is plain input delivered straight to the
/// active workflow's inbox — input is just IO, no cycle.
///
/// Called from [`SessionRuntime::prompt`]. Non-blocking: the channel
/// sends return immediately; the driver and the workflow body pick the
/// work up on their own tasks.
pub(crate) fn dispatch_input<Io: frances_workflow::WorkflowIo>(
    runtime: &SessionRuntime<Io>,
    text: &str,
) {
    match parse_slash_command(text) {
        Ok(Some((name, args))) => {
            let _ = runtime.workflow_cmd.send(DriverCmd::Push {
                name: name.to_owned(),
                args,
            });
        }
        Ok(None) => {
            runtime.workflow_stack.deliver(InboxItem::Input(UserInput {
                content: text.to_owned(),
            }));
        }
        Err(error) => {
            runtime
                .events
                .send(StreamFrame::Error(format!("bad workflow args: {error}")));
        }
    }
}

/// Deliver an interrupt to the active workflow's inbox.
pub(crate) fn dispatch_interrupt<Io: frances_workflow::WorkflowIo>(runtime: &SessionRuntime<Io>) {
    runtime.workflow_stack.deliver(InboxItem::Interrupt);
}

/// Boot-time entry. Either restores the persisted stack (hydrating
/// the row with `active = 1`) or, if the table is literally empty,
/// seats the configured `default_workflow`. Returns the instance to
/// seat as the driver's initial active workflow (or `None` when the
/// stack is empty / nothing hydrated).
///
/// Errors during hydration (missing config, migration drift, runtime
/// error) cascade until a row hydrates cleanly or the live stack is
/// exhausted. The runtime is always usable when this returns.
pub(crate) async fn restore_or_seed<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
) -> Result<Option<WorkflowInstance>> {
    let db = &runtime.workflow_stack.db;

    if row_count(db).await? == 0 {
        let default_workflow = runtime.default_workflow.get();
        let Some(name) = default_workflow.as_deref().and_then(|opt| opt.as_deref()) else {
            return Ok(None);
        };
        match push_default_workflow(runtime, name).await {
            Ok(instance) => return Ok(instance),
            Err(error) => {
                warn!(%error, workflow = %name, "default_workflow start failed");
                return Ok(None);
            }
        }
    }

    hydrate_active_or_cascade(runtime).await
}

/// Push the configured default workflow with empty args. Used by
/// `restore_or_seed` when the table is empty (first-ever boot or a
/// fresh session). Returns the started instance for the driver to seat;
/// `Ok(None)` when no matching config entry exists. A boot failure
/// (migration read or runtime start) propagates as `Err` — `restore_or_seed`
/// logs it and leaves the stack empty. Frames the workflow emits during
/// top-level evaluation buffer in `WorkflowHandle::frames` and flush once
/// the driver starts pumping.
async fn push_default_workflow<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    name: &str,
) -> Result<Option<WorkflowInstance>> {
    let workflows = runtime.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        warn!(
            workflow = name,
            "default_workflow is set but no matching [workflows.*] entry exists; \
             leaving stack empty"
        );
        return Ok(None);
    };
    let instance_id = Uuid::new_v4();
    let instance = boot_instance(runtime, cfg, instance_id, Vec::new()).await?;
    insert_pushed_row(&runtime.workflow_stack.db, name, instance_id, &[]).await?;
    Ok(Some(instance))
}

/// Slash-command push: start the new workflow, then (only on success)
/// dehydrate `old`, persist the row, and tell the TUI to replay the new
/// instance's scrollback. Returns the new `current` for the driver:
/// `Some(new)` on success, or `old` unchanged if the push aborted (so a
/// failed start never leaves the stack empty).
async fn push<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    old: Option<WorkflowInstance>,
    name: &str,
    args: Vec<String>,
) -> Option<WorkflowInstance> {
    let workflows = runtime.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        runtime
            .events
            .send(StreamFrame::Error(format!("unknown workflow: {name}")));
        return old;
    };

    let instance_id = Uuid::new_v4();

    // Boot the new instance BEFORE touching any state. If migrations or
    // start fail, the previous workflow keeps running and the DB is
    // unchanged.
    let instance = match boot_instance(runtime, cfg, instance_id, args.clone()).await {
        Ok(instance) => instance,
        Err(error) => {
            runtime
                .events
                .send(StreamFrame::Error(format!("workflow: {error}")));
            return old;
        }
    };

    // Commit to the push: dehydrate the old workflow (bounded by
    // `DEHYDRATE_TIMEOUT`; the body's `lifecycle.shutdown` runs first).
    if let Some(old) = old
        && let Err(error) = dehydrate(runtime, old).await
    {
        warn!(%error, "dehydrate during push failed");
    }

    // Persist the new row (truncates any non-completed rows above the
    // demoted top — defensive against crash-mid-pop).
    if let Err(error) =
        insert_pushed_row(&runtime.workflow_stack.db, name, instance_id, &args).await
    {
        warn!(%error, "insert_pushed_row failed");
    }

    // Tell the TUI to drop the previous workflow's in-memory scrollback
    // and replay the new active instance's.
    if let Err(error) = crate::scrollback::replay_to_channel(
        &runtime.events,
        &runtime.workflow_stack.db,
        instance_id,
    )
    .await
    {
        warn!(%error, "scrollback replay on push failed");
    }

    Some(instance)
}

/// The long-lived workflow driver. Owns the active instance for its
/// in-memory life and plays the host side of a classical event loop:
/// continuously pump the body's `HostFrame`s to the TUI, watch for
/// genuine termination (`done` — the top-level promise settled or
/// `exit()`), and apply stack-lifecycle commands (slash pushes).
///
/// Input and interrupts do NOT pass through here — they're delivered
/// straight to the active inbox via [`WorkflowStack::deliver`], so the
/// body receives them whenever its JS loop reads, mid-turn or not.
pub(crate) async fn run_driver<Io: frances_workflow::WorkflowIo>(
    runtime: Arc<SessionRuntime<Io>>,
    mut cmd_rx: UnboundedReceiver<DriverCmd>,
    initial: Option<WorkflowInstance>,
) {
    enum Step {
        Transcript(SectionTranscript),
        Surface(SurfaceCmd),
        Permission(PermissionRequest),
        Usage(frances_models_llm::Usage),
        Done(Option<WorkflowError>),
        Push { name: String, args: Vec<String> },
        Shutdown,
    }

    let mut current = initial;
    if let Some(instance) = current.as_ref() {
        runtime.workflow_stack.set_active(instance);
    }

    loop {
        let Some(instance) = current.as_mut() else {
            // No active workflow: only a push can make progress.
            match cmd_rx.recv().await {
                Some(DriverCmd::Push { name, args }) => {
                    current = push(&runtime, None, &name, args).await;
                    match current.as_ref() {
                        Some(inst) => runtime.workflow_stack.set_active(inst),
                        None => runtime.workflow_stack.clear_active(),
                    }
                }
                None => return,
            }
            continue;
        };

        // Drain queued transcript deltas first so a burst doesn't starve
        // the select. The other outputs are independent — the select
        // picks them up. Only the transcript persists, so it's the one
        // that must not back up.
        while let Ok(delta) = instance.handle.outputs.transcript.try_recv() {
            if let Err(error) = emit_transcript(&runtime, &mut instance.emit, delta).await {
                warn!(%error, "transcript emit failed");
            }
        }

        let step = tokio::select! {
            biased;
            delta = instance.handle.outputs.transcript.recv() => match delta {
                Some(d) => Step::Transcript(d),
                None => Step::Shutdown,
            },
            surface = instance.handle.outputs.surfaces.recv() => match surface {
                Some(s) => Step::Surface(s),
                None => Step::Shutdown,
            },
            ask = instance.handle.outputs.permissions.recv() => match ask {
                Some(a) => Step::Permission(a),
                None => Step::Shutdown,
            },
            usage = instance.handle.outputs.usage.recv() => match usage {
                Some(u) => Step::Usage(u),
                None => Step::Shutdown,
            },
            done = &mut instance.handle.done => match done {
                Ok(Err(e)) => Step::Done(Some(e)),
                Ok(Ok(())) => Step::Done(None),
                Err(error) => {
                    warn!(%error, "workflow done channel closed without value");
                    Step::Done(None)
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(DriverCmd::Push { name, args }) => Step::Push { name, args },
                None => Step::Shutdown,
            },
        };

        match step {
            Step::Transcript(delta) => {
                let instance = current.as_mut().expect("active while pumping");
                if let Err(error) = emit_transcript(&runtime, &mut instance.emit, delta).await {
                    warn!(%error, "transcript emit failed");
                }
            }
            Step::Surface(status) => emit_surface(&runtime, status),
            Step::Permission(ask) => emit_permission(&runtime, ask).await,
            Step::Usage(usage) => emit_usage(&runtime, usage),
            Step::Done(reported) => {
                let mut instance = current.take().expect("active on done");
                runtime.workflow_stack.clear_active();
                if let Err(error) = finish_done(&runtime, &mut instance, reported).await {
                    warn!(%error, "workflow done handling failed");
                }
                let instance_id = instance.handle.instance;
                // Drop the in-memory state so its task is gone before we
                // rehydrate the next row.
                drop(instance);
                match drop_active_and_promote(&runtime, instance_id).await {
                    Ok(next) => current = next,
                    Err(error) => {
                        warn!(%error, "pop/promote failed");
                        current = None;
                    }
                }
                if let Some(inst) = current.as_ref() {
                    runtime.workflow_stack.set_active(inst);
                }
            }
            Step::Push { name, args } => {
                let old = current.take();
                runtime.workflow_stack.clear_active();
                current = push(&runtime, old, &name, args).await;
                match current.as_ref() {
                    Some(inst) => runtime.workflow_stack.set_active(inst),
                    None => runtime.workflow_stack.clear_active(),
                }
            }
            Step::Shutdown => return,
        }
    }
}

/// Drain every transcript delta currently queued on `instance`, emitting
/// each into the event stream. `try_recv` releases its borrow on the
/// transcript channel before `emit_transcript` touches the disjoint
/// `instance.emit`, so a single `&mut instance` works under NLL.
async fn drain_transcript<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    instance: &mut WorkflowInstance,
) -> Result<()> {
    while let Ok(delta) = instance.handle.outputs.transcript.try_recv() {
        emit_transcript(runtime, &mut instance.emit, delta).await?;
    }
    Ok(())
}

/// Genuine-termination handling for the active instance: drain any tail
/// frames, emit a clean `BlockStop` for every still-open block, and
/// surface a workflow error if the body settled with one.
async fn finish_done<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    instance: &mut WorkflowInstance,
    reported: Option<WorkflowError>,
) -> Result<()> {
    drain_transcript(runtime, instance).await?;
    instance
        .emit
        .close_all(CloseMode::Stop(&runtime.events))
        .await?;
    if let Some(error) = reported {
        let msg = format!("workflow: {error}");
        instance.emit.persist_error(&msg).await?;
        runtime.events.send(StreamFrame::Error(msg));
    }
    Ok(())
}

/// The side-effect-free core every boot path shares: read migrations,
/// build the invocation, start the runtime, wrap it in a
/// [`WorkflowInstance`]. Touches no stack table and emits no frames —
/// the caller owns instance-id minting, row persistence, and error
/// policy.
async fn boot_instance<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    cfg: &WorkflowConfig,
    instance_id: Uuid,
    args: Vec<String>,
) -> Result<WorkflowInstance, WorkflowError> {
    let migrations = load_migrations(cfg).await?;
    let invocation = Invocation {
        source_path: cfg.file.clone(),
        args,
        entity: cfg.id,
        instance_id,
        migrations,
    };
    let handle = runtime.workflow_runtime.start(invocation).await?;
    Ok(WorkflowInstance {
        handle,
        emit: EmitState::new(runtime.workflow_stack.db.clone(), instance_id),
    })
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
        let resolved = resolve_relative(path, Some(&base));
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
async fn dehydrate<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    mut instance: WorkflowInstance,
) -> Result<()> {
    instance.handle.request_shutdown();
    let deadline = tokio::time::sleep(DEHYDRATE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        // Flush any queued transcript first — it's the only output that
        // persists, so it must reach scrollback before we suspend.
        // Surfaces/usage are ephemeral and a pending permission is
        // resolved by the body's own shutdown path (`closed_notify`).
        drain_transcript(runtime, &mut instance).await?;
        tokio::select! {
            biased;
            Some(delta) = instance.handle.outputs.transcript.recv() => {
                emit_transcript(runtime, &mut instance.emit, delta).await?;
            }
            done = &mut instance.handle.done => {
                // Drain any tail transcript the lifecycle hook pushed
                // immediately before settling.
                drain_transcript(runtime, &mut instance).await?;
                // Body exited cleanly: every remaining open block gets
                // a clean `BlockStop` event and a non-truncated row.
                instance
                    .emit
                    .close_all(CloseMode::Stop(&runtime.events))
                    .await?;
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
                // truncated. No `BlockStop` event — the TUI is about to
                // be told to clear via `ScrollbackFrame::Reset` by the caller's push path.
                instance.emit.close_all(CloseMode::Truncate).await?;
                return Ok(());
            }
        }
    }
}

/// Project a transcript delta onto the `StreamFrame` event stream and
/// persist closed blocks to scrollback.
async fn emit_transcript<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    state: &mut EmitState,
    delta: SectionTranscript,
) -> Result<()> {
    match delta {
        SectionTranscript::Set {
            id,
            section: SectionSpec { kind, seed },
        } => {
            // Error is side-channel (not a streaming section).
            if matches!(kind, SectionKind::Error) {
                let content = seed.unwrap_or_default();
                state.persist_error(&content).await?;
                runtime.events.send(StreamFrame::Error(content));
                return Ok(());
            }
            // One-shot kinds: emit Append + Close + persist immediately.
            if is_one_shot(&kind) {
                let delta = match &kind {
                    SectionKind::Json { tag, value } => {
                        let body = serde_json::to_string(value)
                            .unwrap_or_else(|_| "<unserializable>".into());
                        format!("[{tag}] {body}")
                    }
                    _ => String::new(),
                };
                runtime.events.send(StreamFrame::SectionAppend {
                    id,
                    kind: kind.clone(),
                    delta: delta.clone(),
                });
                runtime.events.send(StreamFrame::SectionClose { id });
                crate::scrollback::persist_section(
                    &state.db,
                    state.instance_id,
                    kind,
                    delta,
                    false,
                )
                .await?;
                return Ok(());
            }
            // Streaming kinds: Set is either an opener or a metadata update.
            let initial = seed.unwrap_or_default();
            let materialised = !initial.is_empty();
            if let Some(open) = state.open.get_mut(&id) {
                open.kind = kind.clone();
                runtime.events.send(StreamFrame::SectionAppend {
                    id,
                    kind,
                    delta: String::new(),
                });
            } else {
                runtime.events.send(StreamFrame::SectionAppend {
                    id,
                    kind: kind.clone(),
                    delta: initial.clone(),
                });
                state.open.insert(
                    id,
                    OpenSection {
                        kind,
                        text: initial,
                        materialised,
                    },
                );
            }
        }
        SectionTranscript::Append { id, delta } => {
            if let Some(open) = state.open.get_mut(&id) {
                if !delta.is_empty() {
                    open.text.push_str(&delta);
                    open.materialised = true;
                }
                let kind = open.kind.clone();
                runtime
                    .events
                    .send(StreamFrame::SectionAppend { id, kind, delta });
            }
        }
        SectionTranscript::Close { id } => {
            state.close_one(&runtime.events, id).await?;
        }
    }
    Ok(())
}

/// True for [`SectionKind`] variants that don't stream — the workflow
/// pushes them once and they're sealed in the same batch.
fn is_one_shot(kind: &SectionKind) -> bool {
    matches!(
        kind,
        SectionKind::ToolUse { .. } | SectionKind::Json { .. } | SectionKind::Diff { .. }
    )
}

/// Workflow-declared chrome (the footer busy indicator). Ephemeral —
/// never persisted; forwarded to the TUI as-is.
fn emit_surface<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    cmd: SurfaceCmd,
) {
    runtime.events.send(StreamFrame::Surface(cmd));
}

/// LLM token-usage telemetry. Pass-through to the TUI footer; not persisted.
fn emit_usage<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    usage: frances_models_llm::Usage,
) {
    runtime.events.send(StreamFrame::Usage(usage));
}

/// A permission request. When `allow_auto`, consult the auto-judge first
/// and answer on the embedded reply slot on approve; otherwise (or on
/// reject/indeterminate) forward to the TUI for a human decision.
async fn emit_permission<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    request: PermissionRequest,
) {
    if request.allow_auto {
        let outcome = crate::runtime::auto_judge::judge(runtime, &request).await;
        match outcome {
            crate::runtime::auto_judge::JudgeOutcome::Approve { reason } => {
                if request
                    .reply
                    .send(frances_workflow::PermissionResponse::Yes {
                        details: Some(reason),
                    })
                    .is_err()
                {
                    warn!("auto-judge approve: workflow stopped waiting");
                }
            }
            crate::runtime::auto_judge::JudgeOutcome::Reject { reason }
            | crate::runtime::auto_judge::JudgeOutcome::Indeterminate { reason } => {
                tracing::debug!(%reason, "auto-judge fell through to user");
                runtime.events.send(StreamFrame::Permission(request));
            }
        }
    } else {
        runtime.events.send(StreamFrame::Permission(request));
    }
}

// --- Persistence helpers --------------------------------------------------

/// SQL helper: tombstone the row matching `instance_id` and promote
/// the next live row to `active = 1`. Then hydrate the new top in
/// memory (if any). If hydration fails, recurse — tombstoning the
/// failed row's branch — until either a row hydrates cleanly or the
/// live stack is exhausted (top stays `None`).
async fn drop_active_and_promote<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    instance_id: Uuid,
) -> Result<Option<WorkflowInstance>> {
    mark_completed_and_promote(&runtime.workflow_stack.db, instance_id).await?;
    let promoted = hydrate_active_or_cascade(runtime).await?;
    // Tell the TUI to clear scrollback and replay the newly-promoted
    // workflow's history (if any row was promoted). When the stack ran
    // dry there's no instance to replay — we still emit an empty reset
    // so the previous workflow's in-memory scrollback is dropped.
    if let Some(instance) = promoted.as_ref() {
        crate::scrollback::replay_to_channel(
            &runtime.events,
            &runtime.workflow_stack.db,
            instance.handle.instance,
        )
        .await?;
    } else {
        runtime
            .events
            .send(StreamFrame::Scrollback(ScrollbackFrame::Reset {
                instance_id: Uuid::nil(),
            }));
        runtime
            .events
            .send(StreamFrame::Scrollback(ScrollbackFrame::End));
    }
    Ok(promoted)
}

/// Find the row with `active = 1` and hydrate it. On any failure,
/// tombstone the row + everything at or above its position and promote
/// the next live row; retry. Loops until a row hydrates (returns
/// `Some`) or the stack runs dry (`None`).
async fn hydrate_active_or_cascade<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
) -> Result<Option<WorkflowInstance>> {
    let db = &runtime.workflow_stack.db;
    loop {
        let Some(row) = read_active_row(db).await? else {
            return Ok(None);
        };

        match hydrate(runtime, &row).await {
            Ok(instance) => return Ok(Some(instance)),
            Err(error) => {
                warn!(
                    instance = %row.instance_id,
                    config = %row.config_key,
                    %error,
                    "workflow restore failed; tombstoning and trying next"
                );
                truncate_at_or_above(db, row.position).await?;
                }
        }
    }
}

/// Attempt to hydrate a single row: look up its config, load
/// migrations, start the runtime with the row's `instance_id`
/// preserved.
async fn hydrate<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    row: &StackRow,
) -> Result<WorkflowInstance, WorkflowError> {
    let workflows = runtime.workflows.get_or_default();
    let cfg = workflows
        .get(&row.config_key)
        .ok_or_else(|| WorkflowError::ScriptCaught {
            context: "restore",
            detail: format!("no [workflows.{}] entry in config", row.config_key),
        })?;
    boot_instance(runtime, cfg, row.instance_id, row.args.clone()).await
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

#[cfg(test)]
mod tests {
    //! Unit tests for the workflow_stack persistence helpers.
    //!
    //! These exercise the SQL layer in isolation against a fresh
    //! in-memory turso connection — no runtime, no `ServerState`. The
    //! end-to-end hydrate/dehydrate path is covered by the workflow
    //! runtime's own test suite plus exercises the rest of the session's other
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
        // simulates a crash where the runtime went down mid-pop after
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

        mark_completed_and_promote(&db, c).await.unwrap();
        assert_eq!(flags_for(&db, c).await, Some((false, false)));
        assert_eq!(flags_for(&db, b).await, Some((true, true)));

        insert_pushed_row(&db, "d", d, &[]).await.unwrap();
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
