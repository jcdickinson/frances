# Agentic loop

Status: replacement architecture, not yet implemented. This overview replaces the
earlier proposal to embed planning, gates, and plan storage in Frances's session
runtime.

Implementation is staged: first replace the JS runtime with an ordinary Rust
coding harness without MCP support or the planning workflow. Later add MCP and
provide the existing workflow as a server. The behavioral record below preserves
the workflow independently of its source.

The [Model Content + Hooks Protocol](../model-content-hooks-protocol.md) and
[UI specification](../model-content-hooks-ui.md) are the authoritative design
contracts for that later integration, not prerequisites for the initial harness.
Where older documents differ, follow those specifications. They are
drafts; their explicitly open questions remain open.

## Current implementation

The [main workflow behavioral record](main-workflow.md) captures the implementation
in `assets/workflows/main.ts`: a
planning interview, structured plan updates, sequential step execution, referee
review, transcript summaries, and fresh chats after progression or rejection.
It persists workflow state through the JS storage API. It does not yet implement
the explicit revision-bound plan approval UI in the new UI specification.

The embedded workflow runtime and JS chat/tool wrappers currently own substantial
coordination: prompt construction, tool dispatch, streaming, interruptions,
continuation, and UI output. Removing JS means moving necessary host behavior
into Rust, not deleting only the main workflow script.

## Target ownership

| Host owns | MCP server owns |
| --- | --- |
| Ordinary Rust agent loop and model access | Planning interviews and phase logic |
| Tool dispatch, permissions, and worker I/O | Plan validation, step progression, and durable plan state |
| Model history, UI transcript, and context identities | Referee and summarizer prompts and verdict interpretation |
| Fixed tool definitions for each context | Selecting tools when proposing a new context |
| MCP session IDs and protocol receipts | Durable MCP session state |
| Native rendering and routing actual user responses | Semantic UI snapshots and interaction state |

Ordinary chat must work without planning. One selected controller can replace
context; multiple servers can independently provide tools, authorization hooks,
approval judgments, and UI. A separate approval server can use MCP sampling
without becoming the planning controller.

The host services generic sampling requests. It does not need built-in referee,
plan, or summarizer semantics.

## Remove the JS workflow layer

First preserve the existing workflow in the [behavioral record](main-workflow.md)
and replace the embedded runtime with an ordinary Rust harness. This stage has
no MCP support or structured planning workflow. It does not need to wait for the
protocol, UI extension, or server implementation.

Later port the workflow into a supplied planning MCP server. Preserve the planning
interview, atomic plan updates and protected step history,
sequential execution, completion proof, referee approval/rejection, skipping,
transcript summaries, return to planning, context reseeding, and continuation
limits. Move its durable state into the server. Add revision-bound plan approval
through `frances/ui` and select tools only when creating each context, as specified
by the new contracts. These are deliberate improvements to the existing workflow.

Provide server configuration/installation and behavioral coverage when that port
lands. The supplied server remains an intended deliverable, but is not a blocker
for removing JS. The host remains generic; it does not absorb the planning
implementation. The context, planning, and MCP persistence sections below describe
this later stage.

Replace the JS-driven loop with a Rust loop that owns model calls, tool execution,
streaming, cancellation, user input, and safe continuation boundaries. Extract
required native functionality currently behind JS bindings before deleting them.

Remove the QuickJS embedding, TypeScript transpilation, embedded
`frances:v1/*` modules, JS workflow entry points, and script workflow
configuration/installation paths. Do not keep a legacy workflow mode or
compatibility wrapper. Crate placement is an implementation decision; useful Rust
I/O, editor, and permission code survives in the host's native components.

Keep the existing Rust model/provider, worker, edit/anchor, persistence, and UI
infrastructure where applicable. Svelte and frontend JavaScript are not part of
this removal. External MCP servers choose their own implementation language;
Frances does not embed that language.

## Context lifecycle

A host session and a model context are different lifetimes. The controller can
request a fresh context seeded with durable plan information while preserving
the workspace and user-visible transcript. The host settles outstanding calls,
persists transition receipts, and honors interruptions before automatic execution.

Tool definitions are fixed when the context is created. Planning contexts omit
editing tools; execution contexts can select them. An explicit URI list chooses
tools, `[]` chooses none, omission retains the selection, and `"default"`
resolves the host defaults once. No per-call tool-list filtering or in-place
tool toggles are part of this design. Argument-dependent authorization still
applies to selected tools.

The editor's per-context read state resets with the conversation. The shared
anchor engine and persisted anchors remain. See [edit-engine.md](edit-engine.md).

## Planning and approval

The planning server maintains typed state and renders it as a plan artifact.
Its MCP tools enforce plan invariants and submit completion proof. Referee and
summarizer calls use negotiated MCP sampling with explicit evidence.

Leaving planning requests a review of an exact plan revision through
`frances/ui`. Approval, request changes, discussion, and cancellation are
different actions. Discussion leaves approval unresolved; dismissal is not
approval. On acceptance, the controller explicitly requests a fresh execution
context and continuation.

After a step, the server records approval or rejection and proposes the next
context. A user gate after every step is a possible workflow policy, not a
host requirement. An automated referee verdict never substitutes for explicit
user approval of a plan. See [approval and discussion](agentic-loop/gate.md).

## Persistence and reconnection

The host saves `MCP-Session-Id` and protocol receipts. Servers retain required
session IDs and application state indefinitely until explicit deletion, including
across process restarts. There is no extra session attachment handshake.

The server chooses its plan schema and database. Do not add the historical
planning tables below to Frances's per-session turso schema merely to implement
the protocol. Host history and server workflow storage have distinct purposes.
Remote servers receive evidence through protocol events or explicit resources,
not by opening the host's private database or transcript paths.

The host reconciles pending transitions and UI actions after reconnection.
Restoring a local rendering entity does not approve or cancel a pending review.
See [session-runtime.md](session-runtime.md) for current behavior and changes
needed at that boundary.

## Earlier server-feature proposals

These documents retain useful design ideas, not an implementation checklist for
the host or additional requirements of the new protocol:

- [Plan schema](agentic-loop/plan-schema.md): historical server data-model proposal.
- [Recall](agentic-loop/recall.md): optional server-owned recall/search surface.
- [Per-file summaries](agentic-loop/file-summaries.md): optional derived knowledge.
- [Project DB](agentic-loop/project-db.md): optional cross-session server storage.
- [Open questions](agentic-loop/open-questions.md): historical questions, with
  already-resolved architectural decisions identified at the top.

Their old staging, storage locations, and gate defaults do not override the
protocol or UI specifications. Supporting these features does not require
reintroducing an embedded scripting runtime.
