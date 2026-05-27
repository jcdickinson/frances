---
title: frances-session consumer-side workflow→host protocol map
description: Detailed mapping of how frances-session consumes HostFrame from workflow runtime and demuxes to TUI via StreamFrame
tags:
  crate: frances-session
  date: "2026-05-27"
  project: frances
  protocol: host-frame
  status: complete
created: 2026-05-27T09:25:23.209765255-07:00
modified: 2026-05-27T09:25:23.209765255-07:00
---

# frances-session Consumer-Side Workflow→Host Protocol Map

## Overview
The workflow runtime (`frances-workflow`) emits `HostFrame` enum variants over an `UnboundedReceiver<HostFrame>` (`WorkflowHandle.frames`). The session driver (`run_driver`) consumes these and re-maps them onto `StreamFrame` for TUI consumption via `EventsChannel`.

## Key Files
- `/home/jono/Code/frances/crates/frances-session/src/workflows/mod.rs` — driver, emit, dehydrate, frame consumption
- `/home/jono/Code/frances/crates/frances-session/src/events.rs` — StreamFrame/ScrollbackFrame enums
- `/home/jono/Code/frances/crates/frances-session/src/runtime/mod.rs` — SessionRuntime, permissions registry
- `/home/jono/Code/frances/crates/frances-session/src/runtime/auto_judge.rs` — permission auto-approver
- `/home/jono/Code/frances/crates/frances-workflow/src/runtime.rs` — HostFrame enum definition

## Protocol Types
- **HostFrame** (workflow-side): Push, Append, UpdateKind, Close, Usage, Status, Permission
- **StreamFrame** (session-side): BlockDelta, BlockStop, Usage, Status, Error, Permission, Scrollback(...)
- **ScrollbackFrame** (replay sub-protocol): Reset, Block, BlockStop, BlockTruncated, Error, End

## Architecture: WorkflowInstance & EmitState
- **WorkflowInstance** (workflows/mod.rs:189-197): wraps `WorkflowHandle` + `EmitState` + config_key
- **EmitState** (workflows/mod.rs:228-342): tracks open blocks, allocates BlockIds, persists scrollback rows
  - `next_block: u64` — monotonic BlockId allocator
  - `open: HashMap<FrameId, OpenBlock>` — maps workflow FrameId → session BlockId + accumulated text
  - Provides: `close_one()`, `close_all_stop()`, `close_all_truncate()`, `persist_error()`

### Open Block Tracking
- **OpenBlock** (workflows/mod.rs:253-257): `{ id: BlockId, kind: BlockKind, text: Option<String> }`
- Text is buffered until `Close` to persist full body in one scrollback row
- Never-written blocks (pushed with no content, never appended) are not persisted