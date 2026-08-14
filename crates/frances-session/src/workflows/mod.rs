//! Per-session workflow selection.
//!
//! A session owns at most one workflow. The selected workflow is stored in
//! session metadata and restored with the session id as `import.meta.instance`
//! across process restarts. Switching workflows updates that metadata after
//! the new workflow has booted successfully and the old one has shut down.
//!
//! On first boot, empty workflow metadata is seeded from `default_workflow`, which
//! defaults to `"main"` when unset.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use frances_core::resolve_relative;
use parking_lot::Mutex as PlMutex;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::warn;
use uuid::Uuid;

use crate::Result;
use crate::events::StreamFrame;
use crate::runtime::{EventsChannel, SessionRuntime};
use crate::session::SessionWorkflow;
use crate::store::Database;

use frances_storage::Migration;
use frances_workflow::{
    InboxItem, Invocation, PermissionRequest, SectionId, SectionKind, SectionSpec,
    SectionTranscript, SurfaceCmd, UserInput, WorkflowHandle, parse_slash_command,
};
pub use frances_workflow::{Runtime as WorkflowRuntime, WorkflowConfig, WorkflowError};

/// How long to wait for a dehydrating workflow's body to settle after
/// `request_shutdown`. Bounded so a misbehaving `lifecycle.shutdown`
/// hook can't hang a workflow switch.
const DEHYDRATE_TIMEOUT: Duration = Duration::from_secs(5);

/// In-memory access to the currently-hydrated workflow. The driver owns
/// the instance; this struct holds the per-session [`Database`] plus the
/// live wires the rest of the runtime uses to reach it: a clone of its
/// inbox sender and its `instance_id`.
pub struct ActiveWorkflow {
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

impl ActiveWorkflow {
    /// Builds an empty active-workflow handle bound to `db`.
    pub fn new(db: Database) -> Self {
        Self {
            db,
            active_input: PlMutex::new(None),
            active_instance_id: PlMutex::new(None),
        }
    }

    /// Per-session [`Database`] handle. Same lock the rest of the runtime
    /// uses; cheap clone for callers (like scrollback replay) that want
    /// to issue SQL against it without going through this owner.
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
    /// away. No-op when no workflow boots.
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
/// inbox via [`ActiveWorkflow::deliver`]. Only workflow switches need the
/// driver to act.
pub(crate) enum DriverCmd {
    /// Replace the session's selected workflow.
    Switch { name: String, args: Vec<String> },
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
    /// (the UI is about to clear and replay via `ScrollbackFrame::Reset`,
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
/// become a [`DriverCmd::Switch`] (handled by the
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
            let _ = runtime.workflow_cmd.send(DriverCmd::Switch {
                name: name.to_owned(),
                args,
            });
        }
        Ok(None) => {
            runtime.active_workflow.deliver(InboxItem::Input(UserInput {
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
    runtime.active_workflow.deliver(InboxItem::Interrupt);
}

/// Boot-time entry. Restores the selected workflow, or starts the
/// configured `default_workflow` when this session has never selected
/// one. `default_workflow` defaults to `"main"` when unset.
pub(crate) async fn restore_or_start_default<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
) -> Result<Option<WorkflowInstance>> {
    if runtime.session.meta.workflow.is_none() {
        let default_workflow = runtime.default_workflow.get();
        let name = default_workflow
            .as_deref()
            .and_then(|opt| opt.as_deref())
            .unwrap_or("main");
        match start_default_workflow(runtime, name).await {
            Ok(instance) => return Ok(instance),
            Err(error) => {
                warn!(%error, workflow = %name, "default_workflow start failed");
                return Ok(None);
            }
        }
    }

    hydrate_selected(runtime).await
}

/// Start the configured default workflow with empty args. Used by
/// `restore_or_start_default` when the table is empty (first-ever boot or a
/// fresh session). Returns the started instance for the driver to seat;
/// `Ok(None)` when no matching config entry exists. A boot failure
/// (migration read or runtime start) propagates as `Err`. Frames the workflow
/// emits during top-level evaluation buffer in `WorkflowHandle::frames` and
/// flush once the driver starts pumping.
async fn start_default_workflow<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    name: &str,
) -> Result<Option<WorkflowInstance>> {
    let workflows = runtime.workflows.get_or_default();
    let Some(cfg) = workflows.get(name) else {
        warn!(
            workflow = name,
            "default_workflow is set but no matching [workflows.*] entry exists; \
             leaving session without an active workflow"
        );
        return Ok(None);
    };
    let instance_id = session_instance_id(runtime);
    let instance = boot_instance(runtime, cfg, instance_id, Vec::new()).await?;
    write_session_workflow(runtime, name, &[])?;
    Ok(Some(instance))
}

/// Workflow switch: start the new workflow, then (only on success)
/// dehydrate `old`, persist the row, and tell the UI to replay the new
/// instance's scrollback. Returns the new `current` for the driver:
/// `Some(new)` on success, or `old` unchanged if the switch aborted.
async fn switch_workflow<Io: frances_workflow::WorkflowIo>(
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

    let instance_id = session_instance_id(runtime);

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

    // Commit to the switch: dehydrate the old workflow (bounded by
    // `DEHYDRATE_TIMEOUT`; the body's `lifecycle.shutdown` runs first).
    if let Some(old) = old
        && let Err(error) = dehydrate(runtime, old).await
    {
        warn!(%error, "dehydrate during switch failed");
    }

    if let Err(error) = write_session_workflow(runtime, name, &args) {
        warn!(%error, "write session workflow failed");
    }

    // Tell the UI to drop the previous workflow's in-memory scrollback
    // and replay the new active instance's.
    if let Err(error) = crate::scrollback::replay_to_channel(
        &runtime.events,
        &runtime.active_workflow.db,
        instance_id,
    )
    .await
    {
        warn!(%error, "scrollback replay on switch failed");
    }

    Some(instance)
}

/// The long-lived workflow driver. Owns the active instance for its
/// in-memory life and plays the host side of a classical event loop:
/// continuously pump the body's `HostFrame`s to the UI, watch for
/// genuine termination (`done` — the top-level promise settled or
/// `exit()`), and apply workflow switches.
///
/// Input and interrupts do NOT pass through here — they're delivered
/// straight to the active inbox via [`ActiveWorkflow::deliver`], so the
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
        Switch { name: String, args: Vec<String> },
        Shutdown,
    }

    let mut current = initial;
    if let Some(instance) = current.as_ref() {
        runtime.active_workflow.set_active(instance);
    }

    loop {
        let Some(instance) = current.as_mut() else {
            // No active workflow: only a switch can make progress.
            match cmd_rx.recv().await {
                Some(DriverCmd::Switch { name, args }) => {
                    current = switch_workflow(&runtime, None, &name, args).await;
                    match current.as_ref() {
                        Some(inst) => runtime.active_workflow.set_active(inst),
                        None => runtime.active_workflow.clear_active(),
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
                Some(DriverCmd::Switch { name, args }) => Step::Switch { name, args },
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
                runtime.active_workflow.clear_active();
                if let Err(error) = finish_done(&runtime, &mut instance, reported).await {
                    warn!(%error, "workflow done handling failed");
                }
                drop(instance);
                current = None;
            }
            Step::Switch { name, args } => {
                let old = current.take();
                runtime.active_workflow.clear_active();
                current = switch_workflow(&runtime, old, &name, args).await;
                match current.as_ref() {
                    Some(inst) => runtime.active_workflow.set_active(inst),
                    None => runtime.active_workflow.clear_active(),
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
/// [`WorkflowInstance`]. Touches no session metadata and emits no frames —
/// the caller owns instance id selection, metadata persistence, and error
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
        emit: EmitState::new(runtime.active_workflow.db.clone(), instance_id),
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
                // truncated. No `BlockStop` event — the UI is about to
                // be told to clear via `ScrollbackFrame::Reset` by the switch path.
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

/// Workflow-declared chrome, published as Session-entity state. The
/// footer commands are ephemeral; `SetTitle` also lands in session
/// metadata (the registry seeds a booting workflow's `getTitle`).
fn emit_surface<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    cmd: SurfaceCmd,
) {
    match cmd {
        SurfaceCmd::SetFooter { text } => {
            runtime
                .registry
                .update_session(|session| session.busy = Some(text));
        }
        SurfaceCmd::ClearFooter => {
            runtime
                .registry
                .update_session(|session| session.busy = None);
        }
        SurfaceCmd::SetTitle { title } => {
            if let Err(error) = runtime.session.write_title(title.clone()) {
                warn!(%error, "persist session title failed");
            }
            runtime
                .registry
                .update_session(|session| session.title = title);
        }
    }
}

/// LLM token-usage telemetry. Session-entity state; not persisted.
fn emit_usage<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    usage: frances_models_llm::Usage,
) {
    runtime
        .registry
        .update_session(|session| session.usage = Some(usage));
}

/// A permission request. When `allow_auto`, consult the auto-judge first
/// and answer on the embedded reply slot on approve; otherwise (or on
/// reject/indeterminate) forward to the UI for a human decision.
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

/// Hydrate the session's selected workflow. If it cannot start, leave
/// the metadata in place and run without an active workflow.
async fn hydrate_selected<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
) -> Result<Option<WorkflowInstance>> {
    let Some(workflow) = runtime.session.meta.workflow.as_ref() else {
        return Ok(None);
    };
    let selection = WorkflowSelection {
        config_key: workflow.name.clone(),
        instance_id: session_instance_id(runtime),
        args: workflow.args.clone(),
    };

    match hydrate(runtime, &selection).await {
        Ok(instance) => Ok(Some(instance)),
        Err(error) => {
            warn!(
                instance = %selection.instance_id,
                config = %selection.config_key,
                %error,
                "workflow restore failed"
            );
            Ok(None)
        }
    }
}

/// Attempt to hydrate the selected workflow: look up its config, load
/// migrations, start the runtime with the session `instance_id`
/// preserved.
async fn hydrate<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    selection: &WorkflowSelection,
) -> Result<WorkflowInstance, WorkflowError> {
    let workflows = runtime.workflows.get_or_default();
    let cfg = workflows
        .get(&selection.config_key)
        .ok_or_else(|| WorkflowError::ScriptCaught {
            context: "restore",
            detail: format!("no [workflows.{}] entry in config", selection.config_key),
        })?;
    boot_instance(runtime, cfg, selection.instance_id, selection.args.clone()).await
}

struct WorkflowSelection {
    config_key: String,
    instance_id: Uuid,
    args: Vec<String>,
}

fn write_session_workflow<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
    name: &str,
    args: &[String],
) -> Result<()> {
    runtime.session.write_workflow(SessionWorkflow {
        name: name.to_owned(),
        args: args.to_vec(),
    })
}

fn session_instance_id<Io: frances_workflow::WorkflowIo>(
    runtime: &Arc<SessionRuntime<Io>>,
) -> Uuid {
    Uuid::parse_str(&runtime.session.id).expect("session ids are UUIDs")
}
