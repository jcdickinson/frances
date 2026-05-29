# Warn on unbounded open-section buffers

When a workflow opens a scrollback section and streams into it without ever
closing, `EmitState.open` keeps the accumulated body in memory in
`OpenSection.text` (`crates/frances-session/src/workflows/mod.rs:251-258`). The
buffer is by design — on close we persist the whole body as one scrollback row,
so it has to be held until the `SectionClose`.

In normal operation this is bounded: few sections are open at once and each is
cleaned on close (and `close_all_*` closes everything on workflow termination).
The only way it grows without bound is a misbehaving workflow that opens a
section and streams forever without closing it.

## Idea (low priority)

We don't really want to engineer around dumb workflows — but if it's cheap, emit
a **warning** when an open section's buffered `text` crosses some size threshold
(or has been open for a long time / many appends). Surface it as a diagnostic so
the *author* of the workflow notices, rather than silently growing memory.

Explicitly **not** doing: hard caps, truncation, or watchdog eviction of live
sections. The buffer is needed data; the goal here is visibility, not enforcement.

Moved out of `docs/plan/review-efficiency.md` — it isn't an efficiency fix, it's
a possible future hardening/diagnostic.
