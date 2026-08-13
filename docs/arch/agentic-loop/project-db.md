# Project DB

The project DB is a per-workdir database that accumulates knowledge across sessions. It is the answer to "what did I learn about this codebase last week?" — a question that the per-session DB cannot answer because each session starts empty.

This is layer 3 in the staged rollout (see [agentic-loop.md](../agentic-loop.md)). Foundation (layer 1) and per-file summaries (layer 2) ship first. Layer 3 should not be designed in detail until 1 and 2 are in use; treat what follows as architectural intent, not a detailed spec.

## Why this layer is separate

Frances's existing design pin: per-session DBs, no `session_id` columns, sessions isolated at the file level. That decision was made to avoid cross-daemon file-lock contention on a global DB ([session-runtime.md](../session-runtime.md) calls this out explicitly). Cross-session knowledge cannot live in the per-session DBs without breaking that design.

Three options were considered:

1. **Federated query across session DBs.** Open all sibling session DBs at query time, union results. Simple, but FTS indexes are per-DB so federated FTS is awkward, connection count grows linearly, performance degrades as sessions accumulate. Doesn't scale.
2. **One global DB, sessions as rows.** Conceptually clean but breaks the per-session-by-design invariant and forces schema migration with `session_id` everywhere. Reintroduces the cross-daemon lock contention that motivated per-session DBs in the first place. Rejected.
3. **Project DB, populated by promotion.** A separate `project.db` lives at the workdir root. On plan completion, *promoted keepers* — findings, decisions, file summaries, plan outcomes — are written into the project DB. Session DBs stay per-session.

(3) wins because it preserves the existing invariant, makes cross-session knowledge explicitly summarized rather than raw, and gives the project DB its own indexes tuned for cross-session queries.

## What gets promoted

On plan completion (or on explicit `remember()` tool call mid-session), these flow into the project DB:

- **Plan outcome** — `{ plan_id, title, prelude.goal, status, summary, completed_at }`. A one-row record of "we attempted X; we reached Y."
- **Findings** — the entire `findings` set from completed steps, with `plan_id` and `step_id` annotations preserved.
- **Decisions** — same shape as findings; choices made about the codebase that should not be relitigated.
- **Open questions** — things left dangling, useful for future sessions to pick up.
- **File summaries** — `current_summary` per file, annotated with `plan_id` so multiple sessions can have entries for the same path without conflict.

What does **not** get promoted:

- Raw transcripts and tool output (too much, drifts fast, privacy risk on a long-lived project DB)
- Step bodies (consumed; not durably useful)
- Anything ephemeral to a single plan's execution
- The plan structure itself in detail (the plan title + outcome is enough; the step-by-step belongs in session)

## Promotion model

Two triggers:

1. **Plan completion.** When a plan reaches `status: completed`, its findings/decisions/file-summaries are promoted automatically. The user can audit at the final gate ("these will move to project memory") and edit before promotion.
2. **Explicit `remember(fact, kind)` mid-session.** A tool the agent or user can invoke to promote a specific finding or decision early — for things that obviously matter for future sessions and shouldn't wait for plan completion.

Don't promote on every step completion. Too aggressive — over-fits the project DB to in-progress work that may yet be reverted.

## Project identity

The project key is the canonical absolute path of the working directory at session start. That works until the user moves the directory.

For stability across moves, support an optional override file at the project root — e.g. `.frances/project-id` containing a stable identifier. If present, use it; if absent, use the canonical cwd. New sessions can be configured to write the override file when first encountered.

This is left explicitly underspecified in this doc; see [open-questions.md](open-questions.md). The right answer probably emerges from real usage.

## Privacy and scope

`~/work/client-a` and `~/work/client-b` must not bleed into each other. Per-cwd keying handles this trivially — a project DB at `client-a/.frances/project.db` has no path through which `client-b` content could reach it.

Cross-machine sync is out of scope. Project DBs are local artifacts, like build caches.

## Concurrent sessions on the same project

Two active Frances sessions both touching `src/llm.rs`. Both eventually complete plans that promote file summaries for the same path.

Do not try to merge them. Each promotion creates a `file_summary` row keyed by `(plan_id, path)` — multiple plans can have entries for the same file. The agent reading the project DB sees them all, with plan IDs and timestamps; if the agent needs to synthesize "what's the current state," that's a query-time task it can perform with full context.

A "latest summary per path" view is achievable as a query (e.g. "most recent `file_summary` per path") — but is a derived view, not stored state. Don't pretend there's one canonical summary.

## Recall surface

A separate tool, not a flag on the per-session `recall`:

```
project_recall(
  kinds: ['finding', 'decision', 'file_summary', 'plan_outcome', 'open_question'][],
  paths?: [str],            // for file_summary kind
  plan_ids?: [int],         // restrict to specific past plans
  since?: timestamp,
) -> [...]

project_search(
  query: str,
  opts?: { kinds?, plan_ids?, since?, limit? }
) -> [Hit]
```

Reasons to keep separate from per-session recall:

- Different DB, different freshness — project DB is older, summarized, less precise
- Different trust model: project DB facts are claims from prior runs, may be stale, must be verified before acting
- Forces the agent to think "am I asking about *this* session or *this project*?" — that distinction is load-bearing

## Trust model

Project DB content is *claims from prior runs*, not *current ground truth*. Specifically:

- A finding from 6 weeks ago about how `src/llm.rs` worked may be obsolete — the file may have changed
- A decision made under earlier constraints may no longer apply
- An open question may have been resolved without anyone updating the project DB
- File summaries can drift from disk (same drift problem as per-session, see [file-summaries.md](file-summaries.md))

The agent's posture toward project DB content should be: "this is a hint, not a fact." Read it for orientation; verify before relying on it. The recall response should include timestamps and source-plan IDs to make this judgment easier.

This is the same posture the harness's auto-memory system already encodes ("memory records can become stale; trust what you observe now") — borrow that model.

## Overlap with harness auto-memory

The harness has its own memory system at `~/.claude/projects/<project>/memory/` with `user`, `feedback`, `project`, and `reference` types. This is *not* the same thing as Frances's project DB.

- **Harness auto-memory** is about the *user* and how to work with them — preferences, role, feedback on past collaboration, references to external systems.
- **Frances project DB** is about the *codebase* from the agent's POV — findings about how the code works, decisions made about it, file evolution.

The two are complementary. They can coexist without integration — different consumers, different data shapes. If a unification is ever attempted, it should be a deliberate design exercise, not an accident. For now: keep them separate, don't try to make the harness read Frances's project DB or vice versa.

## Storage

```
plan_outcomes(plan_id, session_id, title, goal, status, summary, completed_at)
project_findings(id, plan_id, step_id, kind, title, prose, evidence_ref_json, created_at)
                                          -- kind: finding | open_question
project_decisions(id, plan_id, step_id, title, prose, evidence_ref_json, created_at)
project_file_summaries(plan_id, path, current_summary, current_hash, completed_at)
```

`evidence_ref_json` cannot point at session-DB turn IDs in any meaningful way once the session DB is gone (sessions are typically not preserved long-term). Promotion should *flatten* evidence — copying the relevant prose into the project finding, rather than relying on a back-link that may not resolve. This is the trade-off for cross-session: lose precise evidence-back-links, gain durability.

FTS5 indexes over the same agent-readable surface as in-session.

## Garbage collection

Project DBs grow forever. Eventually they need pruning — old plans, stale findings, summaries for deleted files.

Out of scope for this design. A reasonable v1 strategy is "do nothing; assume plans are infrequent enough that a 10-MB project DB is fine for years." If/when GC becomes necessary: by-age (drop everything older than N days), by-status (drop plans with status `abandoned`), or user-driven (UI command "clean up project memory"). Pick when the problem appears, not before.
