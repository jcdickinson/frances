# Main workflow: behavioral record

Status: record of `assets/workflows/main.ts` before removal, captured on
2026-09-23. This document is self-contained so the script and its JS runtime can
be deleted. They have now been removed. It describes their behavior, including limitations, rather than
specifying the ordinary harness that replaces it.

The immediate implementation is a regular Rust coding harness without MCP or
built-in planning. Later, Frances will supply this workflow as an MCP server.
The [protocol](../model-content-hooks-protocol.md) and
[UI extension](../model-content-hooks-ui.md) describe that future integration.
Explicit plan approval is an addition to the future port, not existing behavior.

## User experience

A fresh workflow starts in planning with an empty plan and a ready message asking
the user to describe the task. The agent interviews the user, records a structured
plan, then executes its steps in order. Each successful or skipped step starts a
fresh model conversation seeded with the entire plan. An isolated referee reviews
completion claims. Rejection also starts a fresh conversation, keeping the same
step active and telling the agent what to fix.

The plan is a typed object rendered as Markdown, not a Markdown file that the
workflow writes to the workspace. Its durable home is the session database.
Planning updates post a short indexed list of titles; they do not automatically
display the full plan. There is no dedicated plan panel or approval modal.

## Plan and invariants

The plan contains `title`, `prelude`, ordered `steps`, and an ISO `updatedAt`
timestamp. Initially the title is `Untitled plan` and the other content is empty.
The prelude holds decisions and context that must survive conversation resets.
Each step has a title, execution instructions in `body`, and one status:

| Status | Additional recorded content |
| --- | --- |
| `pending` | None |
| `active` | None |
| `completed` | `summary`, arbitrary JSON `proof`, and `transcript_summary` |
| `skipped` | `reason` |

Completed and skipped steps form a contiguous prefix. There is at most one active
step; if present, it is the first unfinished step. Advancing activates the first
pending step. With no pending steps, no step is active. Steps have no stable IDs:
tools use zero-based indices; rendered headings and progress messages use
one-based numbers.

The active step and terminal prefix cannot be edited, moved, or removed by plan
tools, even after returning to planning. Metadata and pending steps remain
editable. This means returning to planning cannot directly rewrite the active
step's body; new decisions can be placed in the prelude or pending work.

The Markdown rendering contains the title, prelude, every numbered step's title,
status and body, then terminal evidence: summary, fenced proof and transcript
summary for completed steps, or the reason for skipped steps. It ends with the
current step or `none`. An empty plan instead displays a no-execution-steps note.

## Planning and execution prompts

Planning instructs the agent to ask one question at a time, include a recommended
answer, explore decision branches and resolve dependencies. Questions answerable
from the repository should be answered through exploration. The interview prompt
was adapted from Matt Pocock's `grill-me` skill.

Plans use first-person ADR-style prose. Steps contain only work to execute after
planning, not the interview or investigation needed to form the plan. The agent
is told to call `plan_exit` once shared understanding and concrete steps exist.
There is no enforced user approval: the model decides when that condition holds.

Execution instructs the agent to work only on the active step, keep pending work
accurate, submit proof on completion or a reason to skip, and return to planning
when blocked on a user decision. It asks for occasional progress updates.

Both modes attach, in order: the mode prompt, environment information, tool
guidance, global agent instructions, local agent instructions, a nested agent
instruction inventory, and working-directory information. Main chats use the
`chat` model intent and the current effort override.

## Tools

Both modes expose shell run/wait/kill, shell set/get, file read/search, variable
get/set/edit, and `plan_update`. Planning additionally exposes `plan_exit`.
Execution instead exposes `plan_finish_step`, `plan_begin`, and file operations
to replace lines, insert before/after, create a file, and overwrite a file.

Planning omits the dedicated file-edit tools but retains shell execution. It is
not an enforced read-only environment: shell commands can modify files.

Shell, variables and plan tool instances live across context resets. Each new
context creates an editor and associated search/file tool instances, resetting
the editor's read state and shared search loop guard. This does not discard the
underlying filesystem or shared anchor storage. Shell process lifetime across
context resets is distinct from restoring a workflow after process restart.

### `plan_update`

Accepts optional `title`, `prelude`, and an `operations` array. Operations execute
sequentially against a copy of the steps; indices refer to the result of preceding
operations in that call. A failed operation rejects the edit set without applying
earlier operations or metadata changes.

| Operation | Arguments | Meaning |
| --- | --- | --- |
| `add` | `index`, `step: {title, body}` | Insert a pending step; insertion at the end is allowed |
| `update` | `index`, `step: {title, body}` | Replace a pending step's title and body |
| `remove` | `index` | Remove a pending step |
| `move` | `from`, `to` | Remove the step at `from`, then insert it at `to` |

All affected indices must be beyond the protected prefix, including the active
step. Move indices must name existing positions before removal. New titles and
bodies must be nonempty after trimming; plan titles must also be nonempty.
The prelude is free-form text. Success updates the timestamp, posts a zero-based
title list, saves state and returns that confirmation. Failure returns a tool
error.

### `plan_exit`

Accepts an empty object. Rejects an empty plan or one with no remaining work.
Validates the plan, activates the first pending step if necessary, persists a
pending exit flag and returns success. After the model round settles, the loop
switches to execution, clears the step transcript and opens a fresh chat seeded
with the full plan and instructions to start the active step.

### `plan_begin`

Accepts optional strings `goal` and `context`. Records and persists a pending
return to planning. After the round settles, the loop opens a fresh planning chat
with the question to resolve, explicitly carried context, and the full plan.
The plan and active step survive. Resuming requires another `plan_exit`.

### `plan_finish_step`

Requires an active step and accepts one of:

```json
{ "action": "complete", "summary": "What changed", "proof": "Commands and results" }
```

```json
{ "action": "skip", "reason": "Why this step is unnecessary" }
```

Summary/reason must be nonempty strings. The schema requires proof for completion,
but the handler itself does not check its presence or validate its substance.
There is no target step argument. The tool persists a pending signal and returns;
review and advancement happen after the round, not inside the tool handler.

Skipping records the reason and advances without referee or user approval.
Completion invokes the referee. Approval generates a transcript summary, records
the terminal evidence and advances. Rejection leaves the active step unchanged.

## Referee and transcript summary

The referee is an isolated model call with intents `referee`, `cheap`. It receives
a strict lightweight judge prompt, the full rendered plan and the submitted
completion signal. It has only a forced `decide` tool accepting verdict `approve`
or `decline`, with an optional message. The call requests at most one tool call;
forced-tool retries are implemented below the workflow in Rust.

The referee does not receive the full step transcript, inspect the workspace or
run tests. Its evidence is what the plan and completion submission say. Errors,
missing decisions and invalid verdicts all become rejection. A decline without
explanation defaults to insufficient proof.

The workflow separately accumulates step transcript entries: user text, assistant
text, tool arguments and tool results including error status. Reasoning is shown
in its own UI message and retained by provider history handling, but excluded
from this summary input. Text/arguments are truncated to 4,000 JavaScript string
characters per entry; tool results to 8,000, with a truncation note.

After referee approval, a separate tool-free chat with intents `cheap`, `chat`
summarizes the active step, completion signal and accumulated transcript, with
the transcript truncated to 30,000 characters. It is told to retain paths,
commands, results, errors, decisions and learned facts without inventing details.
An empty response or summarizer error falls back to the first 2,000 characters
of transcript; no entries produces an explicit no-transcript message. The summary
is cached and stored on the completed step.

Approval and skipping clear the transcript accumulator. Rejection retains it
across the retry, despite clearing model conversation history. Returning to
planning also retains it initially, but `plan_exit` clears it on resumption.
Skipped steps receive no transcript summary.

## Context transitions

Every transition creates a new chat with fresh file tools, the appropriate prompt
sections, and a synthetic user message. Old model history is not copied into the
new context; persisted history and visible UI messages are not erased.

| Trigger | New mode | Seed |
| --- | --- | --- |
| Exit planning | Execution | Full plan, start/resume the active step |
| Complete with approval | Execution | Completed-step notice, full plan with evidence, next position |
| Skip | Execution | Skipped-step notice, full plan with reason, next position |
| Referee decline | Execution | Decline reason, full plan, instruction to repair the same step |
| Return to planning | Planning | Goal, explicitly carried context, full plan |

Even completing the final step creates a fresh execution chat, which is told to
report the final result when no active step remains. There is no automatic reset
to an empty planning workflow after finishing the plan.

Transition tools set pending flags. The loop consumes them before a model round
and after a completed round, in priority order: planning exit, planning begin,
step completion. These are individual slots, not a queue: repeated submissions
can overwrite a pending value. Other calls in the same round can run before the
transition is consumed. This is not an exactly-once transition protocol.

## Agent loop, input and output

The workflow maintains one outstanding inbox read. Each ordinary user message is
trimmed, displayed, recorded for summarization, saved and pushed into the current
chat. The workflow streams assistant text and reasoning into separate UI messages,
sets status to planning or working, and requests `maxToolCalls: 8` per round.
Tool calls/results are captured and state is saved before and after each wrapped
handler. Tools render their own visible entities, such as shell output or diffs.

Rounds continue while tools are used. Planning can finish its turn with a textual
question. Execution with an active step cannot normally stop with text alone:
the loop injects a reminder to continue, finish/skip, or return to planning.
With no active step, a tool-free response ends the turn.

A new message during a round waits for that round's tools/results to settle,
then becomes the next user turn. Escape aborts streaming and kills the shell;
the chat dispatch layer settles unfinished calls with interrupted results.
Partial assistant history and results are retained. Escape while idle is ignored.
Interruption does not itself clear pending workflow transitions.

On the normal idle return path the editor commits accumulated edits, reconciling
anchors and clearing tombstones. Early returns for interjection/interruption
bypass this call. Chat failures display an error section. The shell closes when
the outer loop exits.

The intended continuation budget is 50 automatic resets/nudges per user turn.
The implementation counts completion/rejection resets, returns to planning and
idle nudges, but not planning exits. There is also a control-flow defect: exceeding
the budget inside pending-signal handling returns false to the caller, which can
still start another round. The direct idle-nudge limit does break the loop.
Preserve bounded continuation in a port, not this accidental escape from it.

Exact `quit` exits with a farewell. `/effort` reports the current override;
`/effort default` clears it; `/effort 0` through `/effort 100` set it. Invalid forms
show usage. These commands are displayed in the UI but do not enter model history.
Effort is saved and applied to newly created main chats.

## Persistence and restart

The JS storage API stores one row per workflow instance in `main_workflow_state`,
with columns `instance_id` (primary key), `version`, `state_json`, and `updated_at`.
Instance identity is `String(import.meta.instance)`. State schema version is 3.

The JSON snapshot contains mode, the entire plan, effort override, current chat ID
and mode, a pending seed, variable-store entries, transcript entries/cached
summary, and pending completion/exit/begin signals. Chat history is stored
separately; saving state first ensures the current chat has a persisted ID.
Shell process state and editor read caches are not part of this JSON snapshot.

Restart loads the same instance's snapshot, rejects unsupported schema versions,
validates the plan, restores variables/transcript/signals and loads the referenced
chat (or creates one if its ID is absent). Prompt sections and fresh tool instances
are attached for the restored mode. A saved pending seed is pushed into the chat.
Successful rounds clear that seed. Restored workflows omit the fresh ready banner
and wait for inbox input; restoration itself does not call the turn loop.

Snapshots and chat persistence are separate operations. Transition handling can
save a new mode before saving the replacement chat. Do not infer atomic crash
recovery or exactly-once effects from these checkpoints. A future server needs its
own durable transition semantics; it need not retain this database schema.

## Later port and verification

The behavior to recover in the supplied server is the interview, durable typed
plan, atomic pending-step operations, protected history, sequential completion
and skipping, referee review, transcript summaries, return to planning and fresh
context seeds. Ordinary harness responsibilities—streaming, dispatch, tool I/O,
interruptions, history and editor lifecycle—belong in Rust independently of that
port. See the [staged replacement](agentic-loop.md#remove-the-js-workflow-layer).

The removed tests in `crates/frances-workflow/src/runtime/tests/main_workflow.rs`
covered restart hydration without a ready banner, effort commands and persistence,
variable-store round trips, and source-level assertions about plan/finish tool
contracts. They are not end-to-end proof of the whole planning loop.

A later port should exercise successful and rejected atomic edits, protected
active/terminal steps, empty/finished planning exit, ordered completion/skip,
referee failure/retry, summary fallback, all context seeds and tool sets,
return-to-planning continuity, interruption/interjection, restart during a
transition, final-step reporting and an effective continuation limit. Explicit
revision-bound user approval belongs to the new UI design and needs additional
coverage. Known limitations above are reference facts, not requirements to copy.
