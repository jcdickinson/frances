# Bind workspace directories to targets and agents

All agent-facing project I/O is worker-backed. Shell execution, file reads and
writes, edits, metadata, canonicalization, find, and grep execute in the
worker. That completed work does not belong in this todo.

The remaining environment work must not extend the current single-directory,
single-worker assumption. Multi-directory workspaces need to land first.

## Ownership model

A workspace is an ordered collection of directory agents. Each entry owns:

- one directory, expressed in that target's path namespace;
- one target and worker connection;
- one agent execution context, including its workflow tools, cwd, editable
  root, shell resources, prompt environment, and instruction chain.

These are logically 1:1:1. A local directory uses a `Local` target which
launches the same worker protocol locally; it is not a separate in-process I/O
mode. Two entries may name the same underlying host, but initially they still
own separate workers and agent contexts. Connection pooling can remain a later
implementation detail if it ever proves useful.

The workspace and its session storage remain frontend-owned containers around
those directory agents. The UI may present them together, but an individual
agent never has to guess which target an absolute or relative path belongs to.

## Prerequisite: multi-directory targets

- Replace the workspace file's path-only directory entries with directory
  entries that bind a path to a target specification. A bare directory opens
  as one entry using `Local`.
- Stop canonicalizing and validating target directories through frontend
  `std::fs`. Launch/connect the entry's worker, then resolve and validate its
  directory through that worker.
- Replace the session-wide worker and `editable_roots = vec![primary_root]`
  collapse with one directory-agent runtime per workspace entry.
- Keep `WorkflowIo` singular inside a directory agent. Construct it from that
  entry's worker instead of adding a target selector to every filesystem and
  shell operation.
- Make the primary directory an ordering/UI default only. It must not make the
  other workspace entries invisible to the runtime.
- Add integration coverage with at least two directories whose workers expose
  different files, path roots, and environments. Include a local target; local
  and non-local targets must use the same runtime path after connection.

## Worker environment per directory agent

After directory-agent ownership exists, add a typed environment snapshot to
the worker protocol. It must contain the target OS/platform and shell plus the
home and XDG paths needed for target-global instruction discovery. Paths stay
paths on the wire; closed domains use enums rather than arbitrary strings.

Retain the snapshot on the directory agent's worker client and expose it to
that agent's workflow dependencies. Do not put it on the session as a whole:
two workspace directories may run on different operating systems.

`InvocationContext::process.env` remains local. It supplies frontend-side
configuration and LLM-provider credentials and is not a worker environment.
`ChatSession._envInfo()` must use its directory agent's worker snapshot for
target OS/platform/shell and must not fall back to frontend process values.

## Instruction chain per directory agent

Each directory agent receives instructions in this precedence order:

1. frontend-global instructions;
2. that directory's target-global instructions;
3. that directory's repository-root instructions;
4. nested instructions within that repository.

Frontend-global candidates come from the frontend's `HOME`,
`XDG_CONFIG_HOME`, and `XDG_CONFIG_DIRS` and are read through an explicit local
control-plane filesystem surface. They may be loaded once and shared across
directory agents.

Target-global candidates come from the worker environment snapshot and are
read through that directory agent's `WorkflowFs`. Repository and nested
instructions continue through the same worker-backed filesystem.

Keep canonical-path and content deduplication within each filesystem.
Deduplicate identical content across the merged frontend/target global chains
without treating equal path strings from different machines as the same file.

## Intentionally local

- workflow source and migration loading;
- Frances configuration, provider credentials, workspace/session storage, and
  the turso database;
- UI/event persistence and other application control-plane state;
- timers and sleep;
- the user-facing current date used in prompt assembly.

## Completion criteria

- Every workspace directory has exactly one target worker and one agent
  execution context.
- Local and non-local directory targets use the same worker-backed tool path.
- No target path is resolved, canonicalized, or validated against the wrong
  machine's filesystem.
- Each agent reports its own target environment; no session-global environment
  is assumed.
- Frontend-global paths are read only from the frontend filesystem. Target and
  repository paths are read only from the owning directory's worker.
- Frontend secrets remain local and are not copied into worker snapshots or
  protocol messages.
- A multi-directory integration test proves file/shell isolation, per-agent
  environment identity, and frontend-global → target-global → repository →
  nested instruction precedence.
