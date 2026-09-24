# Session runtime

Status: ordinary Rust harness and [basic MCP support](mcp.md) implemented. The
planning server and Frances extensions described in the
[protocol](../model-content-hooks-protocol.md) remain future work.

## Agent loop

`frances-session::runtime` owns the model connection, user input, interruptions,
history, permissions, and UI publication. `frances-harness` provides native
filesystem, anchor editing, search, shell, and variable tools. There is no embedded
JavaScript runtime, script workflow configuration, or built-in planning loop.

A model context holds a chat session, fixed tool definitions, an editor read
cache, variables, and shell state. The loop calls the model, executes its tool
batch in order, records every result, commits edit reconciliation, and repeats
until the model stops calling tools. Unknown tools and invalid arguments return
errors before execution. Tool definitions stay unchanged across model requests.

Input received during a turn is queued. Interrupt cancels the model or pending
permission, kills a running shell, and gives remaining calls in the batch an
interrupted result. File operations already underway finish at their safe
boundary. The host records results before stopping; it waits for new user input
to resume. Shutdown uses the same cancellation path.

Shell commands go through the permission gate. The existing auto-judge may approve
an eligible command; rejection or an indeterminate result falls through to the
user. Filesystem writes are restricted to resolved editable roots and require a
read in this context, except atomic new-file creation. This is the initial native
policy, not implementation of the draft authorization extension.

Instruction files and environment information are loaded for the context through
the filesystem adapter. Native producers publish chat, file, shell, and diff UI
content through the existing entity hub and transcript channel.

## Session and workspace

Every launch opens a workspace and creates a fresh session. A workspace is a
directory or a TOML file containing `dirs = ["a", "b"]`; relative entries resolve
against the file's parent. Its primary directory is the initial cwd. The launcher
validates the workspace before detaching and starts the sibling
`frances-worker serve --stdio` process.

Session directories are:

- `state_root/sessions/<id>/`: metadata, `frances.db` (turso), anchors, and log.
- `runtime_root/sessions/<id>/`: ephemeral runtime files.

State roots follow XDG conventions, with `~/.local/state/frances` and
`/tmp/frances-<uid>` fallbacks. Directories are created with mode `0700`.
Metadata contains workspace identity and title; it has no workflow selection.
Editable roots currently come from a marker walk on the primary directory.

## Worker and persistence

Production filesystem, shell, and `stdio` MCP process operations cross the worker's framed stdio
protocol. Content attachments carry file bytes, while feeds carry search and
shell output. Process feeds carry raw stdin/stdout; executable lookup and environment
expansion happen on the worker. `local-stdio` MCP stays on the host. Shell observations drain output concurrently with waiting so the
bounded feed cannot block progress. Dropping a shell feed closes its worker
resource. `RealIo` is available for local tests; the desktop uses `WorkerIo`.

Each session owns its own turso database. Chat inputs and tool outcomes are
persisted before continuation, including on interruption. A failed model request
keeps its inputs queued for retry without duplicating primitive history rows.
The transcript has one sequence of sections across the session, with no workflow
instance column or planning tables.

The `EntityHub` persists snapshots, optional append-only streams, and final
artifacts. Chat messages update snapshots while streaming; shell entities carry
bounded output and a model digest. Transcript sections refer to entities or hold
structured diffs. Startup force-settles entities whose producer died, and queues
entity snapshots before transcript replay.

## MCP integration

The driver combines native tools with a fixed inventory from selected MCP servers.
Presets compose by union; configuration alone never activates a server. Selection
changes settle the current turn, prepare the new inventory, then create a fresh
model conversation and editor read cache. Text history, session transcript, and
the shared anchor engine survive. Retained servers keep their connections.
Failed preparation preserves the previous selection. MCP permissions use the
existing user gate without extending the auto-judge. See [MCP support](mcp.md)
for configuration, transports, resource tools, and user-selected prompts.

The [main workflow record](main-workflow.md) preserves the removed planning
workflow for its later server port. That server will own planning state; the
host will own MCP identities, accepted transitions, receipts, and generic UI.
Fresh launch today is not durable MCP reconnection or session restoration.
