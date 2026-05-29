# Agentic loop

This is a design sketch for how Frances drives the model. It is not implemented; treat it as a design target, not a description of code that exists.

## Why

The default agentic loop most tools ship — single rolling transcript, time- or token-based compaction, free-form prose summaries — has known failure modes:

- **Mid-stream compaction is messy.** Cuts in the middle of half-finished thoughts; summary has to guess what the agent was about to do next.
- **Summary lossiness is unpredictable.** The agent decides what's important to keep *before* knowing what the next step will need (file paths, exact error strings, observed code shapes).
- **The plan becomes gospel.** Once the original reasoning is compacted away, the agent can no longer judge whether the plan is still right — it just executes.
- **Knowledge dies at session end.** Anything learned about the codebase is gone next time you run the tool.

Frances has substrate that fixes most of this — a per-session turso DB that already persists every turn, an in-process session runtime that owns the lifecycle, an existing word-anchor system. The design here leans on that substrate to build something better than the default.

## Shape, end to end

1. **Plan as typed structure.** Not a free-form markdown file — a prelude plus an array of steps with `{ title, body, status, summary, outcome, proof, findings, decisions, open_questions, artifacts }`.
2. **Mandated planning, cheap.** No work happens outside a step. But step creation is one line — no design-doc ceremony for "rename this variable."
3. **Task-completion signal.** A step ends when the agent emits a structured outcome with proof. No outcome → step isn't done → no compaction.
4. **User gate at every step boundary.** Default mode shows the user `{title, summary, outcome, proof}` and asks: continue / discuss / replan / abandon. Full-auto mode suppresses the gate, keeps the structure.
5. **Hot/cold context.** Hot context = prelude + completed step skeletons + current step body. Cold context = full transcripts, tool outputs, prior compactions — all persisted in the session DB, recallable on demand via two tools (`recall` and `search`).
6. **Per-file summaries.** Lateral view (cross-step, per-file) generated at step completion. Each entry back-links to the turn IDs containing the actual diffs. Solves "what's the current state of `src/llm.rs`?" without transcript recall.
7. **Project DB via promotion.** A separate per-workdir DB collects findings, decisions, and file summaries promoted from completed plans. Cross-session knowledge accumulates without polluting per-session storage.

## Sub-documents

- [Plan and step schema](agentic-loop/plan-schema.md) — the typed structure: prelude, steps, findings, decisions, outcome, proof.
- [User gate](agentic-loop/gate.md) — what the user sees at step boundaries; the four actions; full-auto mode; the discuss state.
- [Recall surface](agentic-loop/recall.md) — hot/cold context model, the two-tool recall API, FTS5 indexing, ID strategy.
- [Per-file summaries](agentic-loop/file-summaries.md) — generation pipeline, back-link model, drift handling.
- [Project DB](agentic-loop/project-db.md) — cross-session knowledge, promotion model, project identity, overlap with harness auto-memory.
- [Open questions](agentic-loop/open-questions.md) — things this design did not resolve.

## Staging

Don't ship all of this at once. The recommended order, with each layer earning its complexity on its own before the next is added:

1. **Foundation** — structured plans + steps + gate + per-session recall (recall + search tools, FTS5 indexes). This is the bulk of the value; ship it first, prove it works.
2. **Per-file summaries** — adds the lateral view with back-links. Earns its keep on multi-step plans that revisit the same files.
3. **Project DB** — cross-session knowledge via promotion. Earns its keep once people have run the tool enough that "what did I learn last time" is a real question.

Layer 1 should be designed *anticipating* layers 2 and 3: stable IDs on findings/decisions/file-summaries, evidence back-links via turn IDs, no session-only context bleeding into prose summaries. Those forward-compat moves are cheap during initial design and expensive to retrofit.

## What this replaces

The current `frances` binary has none of this — there's a session, a streaming LLM call, an edit tool, and a transcript. There's no plan structure, no step concept, no checkpoint, no recall, no project knowledge. This design is greenfield within the existing session-runtime/edit-session scaffolding; it does not require rewriting [`docs/arch/session-runtime.md`](session-runtime.md) or [`docs/arch/edit-engine.md`](edit-engine.md), only building on top of them.
