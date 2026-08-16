# Complete remote tool I/O

Production uses `WorkerIo`, but not every tool operation is worker-backed yet.
The shell protocol and basic `WorkflowFs` operations execute in the worker;
file editing and file search still contain direct frontend-side filesystem I/O.

## File editing

`frances-workflow/src/modules/file.rs::write_draft` currently uses `std::fs`
for:

- parent directory creation;
- atomic create-new writes;
- overwrite writes;
- post-write metadata;
- rereading the written file.

Move this work behind `WorkflowFs`. The worker protocol needs an atomic
create-new operation so `WriteMode::CreateNew` cannot race or accidentally
overwrite a file. The edit engine may continue to run in the frontend process;
only filesystem access needs to move to the worker.

## Find and grep

`frances-workflow/src/modules/file_find_or_grep.rs` currently performs local:

- path canonicalization and directory validation;
- ignore-aware directory walking;
- file opening and content searching;
- binary detection;
- metadata collection.

Move the complete find/grep operation into the worker. Sending every visited
file through individual `WorkflowFs` calls would be both chatty and would leave
ignore and filesystem semantics split across machines. Add worker requests and
streamed results for the high-level operation instead.

The worker must preserve the current behavior for ignore files, hidden files,
overrides, result ordering, match limits, binary files, truncation, and paths
outside the primary project root when permission allows them.

## Local implementations

`RealIo`, `RealShell`, and `RealFs` remain useful for tests, but production must
continue to construct `WorkerIo` explicitly. Avoid a production fallback to
local I/O when the worker is unavailable; that could silently operate on the
wrong machine.

Local timers are fine. Sleeping does not observe or mutate remote-machine
state.

## Completion criteria

- No production tool module calls `std::fs`, `tokio::fs`, local process APIs,
  or local path canonicalization for project operations.
- Shell, file read/write/edit, metadata, canonicalization, find, and grep all
  execute in the worker.
- Cancellation closes any associated contents or feeds and stops worker-side
  work.
- Dropping a shell output feed terminates and reaps the remote shell.
- Tests run production tool paths against a worker over `multiplex`, not merely
  against local mock implementations.
