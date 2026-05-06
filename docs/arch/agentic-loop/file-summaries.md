# Per-file summaries

Per-file summaries are a *lateral* view of plan progress: cross-step, per-file. They complement the longitudinal view (the plan as a sequence of steps) and answer queries like "what's been changed in `src/daemon/server.rs` so far?" cheaply, without forcing the agent to recall multiple step transcripts and reconstruct the file's evolution by hand.

This is layer 2 in the staged rollout (see [agentic-loop.md](../agentic-loop.md)). Foundation layer 1 ships without it.

## Why

For multi-step plans that revisit the same files — refactors, debugging that touches several call sites, feature work with cross-cutting changes — the natural question "where is this file at right now?" is awkward to answer from per-step recall alone. The agent would have to pull every step's transcript that touched the file and synthesize. Per-file summaries pre-compute that synthesis.

For single-shot work that touches a file once, the value is small. That's fine — it's cheap to skip when not earned.

## Storage

```
file_summaries(
  plan_id,
  path,                       -- canonical relative path within the project root
  current_summary,            -- "where this file is now" prose
  current_hash,               -- file content hash at last summarization
  updated_at,
  PRIMARY KEY (plan_id, path)
)

file_change_entries(
  id,
  plan_id,
  path,
  step_id,
  turn_ids_json,              -- back-links: where the actual diffs live
  summary,                    -- what changed in this step's worth of edits
  created_at,
  INDEX (plan_id, path, step_id)
)
```

`file_summaries` is "current state" — one row per (plan, path). `file_change_entries` is the addressable evolution log — one row per (step, path) pair where the agent edited the file.

## Generation pipeline

At step completion (the existing checkpoint, before the gate fires):

1. List files touched in this step (`artifacts.files_touched` filtered to `write` ops).
2. For each touched file:
   a. Gather diffs from this step's turns (the actual edit operations recorded in tool calls and their results).
   b. Look up the prior `current_summary` for `(plan_id, path)`.
   c. Gather upcoming-task context: the titles + bodies of remaining pending steps (see "Forward-looking summarization" below).
   d. Call a summarizer model with: `prior current_summary + this step's diffs + upcoming-task context` → produce updated `current_summary` + a one-paragraph entry summarizing this step's changes.
   e. Insert a `file_change_entries` row with `turn_ids` populated from the step's turns.
   f. Update or insert the `file_summaries` row with the new `current_summary` and current file hash.
3. Run all per-file summarizations in parallel — they're independent.
4. Persist before the gate UI renders, so the gate can show updated file state if useful.

**Cost control:**

- Only resummarize files touched in the just-finished step. A 10-step plan that touches 3 files per step ≈ 30 summarizer calls total, parallelized.
- The summarizer prompt is small (prior summary + diffs + upcoming-task context); output is small (updated summary + entry). Cheap-model territory — does not need the same model as the main loop.
- "Multiple LLMs" reading: parallel calls per file, one summarizer model. Not an ensemble.

## Forward-looking summarization

The summarizer is given upcoming-step context (titles + bodies of remaining pending steps) along with the diffs. This shapes what the summary keeps versus drops.

Without forward-looking context, the summarizer has to guess what's load-bearing. It might compress "renamed `TokenCache` → `AuthCache`, added field `expires_at: Instant`" down to "renamed and extended the auth cache" — losing the exact name and field type that step 7 will need to call.

With forward-looking context, the summarizer sees that step 7's body says "extend `AuthCache` to handle refresh tokens" and knows: keep the type name, keep the field name, keep the type. The summary becomes "renamed `TokenCache` → `AuthCache`; added `expires_at: Instant` field tracking absolute expiry." Step 7 starts with the precise vocabulary it needs.

This generalizes: chosen identifiers (types, functions, variables, file paths), API shapes, field names, error variants — anything the agent invented in this step and might call by name in upcoming steps — should land in the summary verbatim if upcoming steps reference the surrounding concept. The forward-looking prompt is what makes that possible.

The same principle applies to per-step `summary` writing (the prose the agent produces at step completion). The agent already has the plan structure in context, so this is partially free — but the system prompt that drives summary writing should explicitly call out "preserve names and shapes that upcoming steps will need" rather than relying on the agent to figure that out.

Caveat: upcoming steps can be rewritten at gates (`replan` action). A summary tuned for the original step 7 may be slightly mis-tuned if step 7 is rewritten. Acceptable — summaries are cheap to regenerate if the gap proves significant, and the cost of retuning is bounded.

## Summarizer scope is bounded

The summarizer summarizes. It does not edit the plan.

It would be tempting to fold plan-patching (renames, decision propagation, vocabulary alignment in upcoming step bodies) into this pass — the summarizer already has all the right context: diffs, prior summary, upcoming step bodies. But patches are *decisions*, not compression, and authority for plan edits should sit with the main (larger) model:

- A cheap summarizer making plan edits means the user can't discuss those edits with the main model — at the gate, "discuss" should engage the same model the user has been working with all along.
- Patches are higher-stakes than summaries; getting them wrong is more costly than a slightly-off summary, and the cheap model is wrong more often.
- Bounding the summarizer's job to "compress information" keeps it small, fast, and easy to evaluate.

Plan patching is therefore a separate pass, performed by the main model, taking the cheap summarizer's output as one of its inputs. See [gate.md](gate.md) — "Patch proposal pass" — for the flow.

## Back-links to turns

The `turn_ids_json` on each `file_change_entries` row is the bridge between cheap summary and ground truth:

- Agent reads cheap summary: "src/daemon/server.rs, step 7: refactored timeout handling to share a single deadline across reconnects"
- Agent gets uncertain about a detail: "what exactly did the timeout refactor change?"
- Agent calls `recall(step_ids=[7], { transcript: { from: <turn_id>, to: <turn_id> } })` using the back-linked turns
- Ground truth comes back

Without back-links, summaries float free of evidence and degrade into folklore. The back-links are not optional.

## Drift handling

Critical caveat: `current_summary` is a claim about *what the agent did*, not *what the file currently is*. The user can edit the file manually outside the agent; another tool can write to the file; the agent itself can edit a file without going through the recorded path.

The `current_hash` field is the drift detector. When the agent calls `recall_files(['src/llm.rs'])`:

1. Compute the file's current hash on disk
2. Compare to `current_hash` in the row
3. If they differ, the response includes `drifted: true` and the agent should re-read the file before relying on the summary
4. If they match, the summary is consistent with disk

The schema is honest about this: the field is named `current_summary` (last known agent state), and the recall response surfaces drift explicitly. The summary is never authoritative for content — only for *the agent's narrative* of how the file got there.

## Recall surface

```
recall_files(
  paths: str | [str],
  fields?: {
    current_summary?: bool,             // default true
    change_log?: bool,                  // all entries for this path in this plan
    log_range?: { from_step, to_step }, // restrict the log
  },
  plan_id?: int,                         // default: current plan
) -> [{
  path,
  current_summary?,
  current_hash,
  on_disk_hash,
  drifted: bool,
  change_log?: [FileChangeEntry],
}]
```

Sibling tool to `recall` and `search` (see [recall.md](recall.md)). Different keying (path vs step_id), different conceptual model — keep separate, don't overload the step-keyed `recall`.

For "which files did I touch in step range X..Y", reach for `search` or `recall(step_ids, {artifacts: true})` — that's a step-keyed query with a file-shaped answer, lives on the step side.

## Reads vs writes

Open question: do reads-without-edits produce `file_change_entries`? Probably no — reads belong in the step's `findings` ("I read `src/llm.rs` and noticed X"), not in the file's evolution log. The file didn't change; only the agent's understanding did. See [open-questions.md](open-questions.md).

## What is *not* a per-file summary

- A whole-codebase index. Per-file summaries are scoped to a plan; they record what the agent did during this plan, not the codebase's full state.
- A replacement for actually reading the file. The agent should always read the live file before non-trivial edits — the summary is for orientation, not for direct manipulation.
- A diff log. Diffs are in the turns the entries back-link to. The summary is the *interpretation* of the diff in plain prose; the diff itself stays in transcript storage.

## Promotion to project DB

When a plan completes, its file summaries are candidates for promotion to the project DB (see [project-db.md](project-db.md)). The promoted form is annotated with the plan ID, so the project DB can show "across all completed plans, here's what's been done to `src/llm.rs`" without losing provenance.
