# Recall surface

Frances's per-session libsql DB already persists every turn — the cold storage substrate exists. The recall surface is the agent-facing API for pulling cold content back into hot context on demand.

## Hot vs cold

**Hot context** is what gets inlined in the next prompt to the model. By default, at the start of step N+1:

- Plan prelude (full)
- For each completed step (1..N): `{ id, title, status, outcome, summary, proof, findings, decisions, open_questions }` — the *skeleton*, not the body, not the transcript
- Current step's body
- Anything the agent explicitly recalled earlier in the current step

That's it. Tool outputs from prior steps, full transcripts, deliberation, raw diffs — all cold.

**Cold context** is everything else, persisted in the session DB:

- Full turn-by-turn transcripts (model output, tool calls, tool results)
- Tool output payloads
- The `body` of completed steps
- Per-file summaries and their change-log entries (see [file-summaries.md](file-summaries.md))

"Eviction" is a misnomer — nothing is deleted. Cold means "not inlined in the next prompt." When the agent needs cold content, it asks for it explicitly via `recall` or `search`.

## Two tools

```
recall(
  step_ids: int | [int],
  fields: {
    body?: bool,
    summary?: bool,
    transcript?: bool | { from?: turn, to?: turn },
    findings?: bool,
    decisions?: bool,
    open_questions?: bool,
    proof?: bool,
    artifacts?: bool,
  }
) -> [Step]   // partial; only requested fields populated
```

```
search(
  query: str,
  opts?: {
    kinds?: ['finding', 'decision', 'summary', 'transcript', 'tool_output'],
    step_ids?: [int],            // restrict to subset
    status?: [Status],           // restrict by step status
    limit?: int,                 // default 10
  }
) -> [{
  step_id,
  kind,
  snippet,                       // FTS-highlighted context
  score,
  anchor,                        // pointer for follow-up recall
}]
```

The composition is grep + cat: `search` finds candidates, `recall` pulls the full thing. The agent learns which to reach for.

## Design choices

**Steps as array.** `recall([3, 5, 7], { transcript: true })` is a real query and one round-trip beats three. Singleton stays cheap.

**Turn numbering.** Each LLM round-trip = one turn within its step; turns are numbered `1..N` within a step. `{ from: 3, to: 7 }` is unambiguous and stable across step splits.

**Allow recalling fields already in the skeleton.** Yes, `summary`, `proof`, `outcome` are already inlined. Allowing them in `recall` is redundant in those cases. Forcing the agent to track what's inlined vs not is mental overhead it'll get wrong. Cheap to allow, expensive to police.

**Empty fields = error, not "return everything."** Forces intentionality. Big transcripts shouldn't slip in by accident.

**No truncation on the recall side.** If the agent asks for a 50k-token transcript, give it 50k tokens. Expensive mistakes are how it learns. The agent has an estimate of context budget; let it manage.

**Scope.** Default to current plan; `plan_id?` opt-in for cross-plan within the same session. Cross-session recall is *not* part of this surface — that lives in the project DB (see [project-db.md](project-db.md)).

**No separate `recall_files`.** Use `search(path, { kinds: ['transcript', 'tool_output'] })` for "where did we touch this file" queries; per-file summary lookup is a sibling tool with its own keying (see [file-summaries.md](file-summaries.md)).

## What gets indexed

At step completion, FTS5 indexes are populated for:

- Summary prose
- Finding titles + prose
- Decision titles + prose (rationale)
- Open question titles + prose
- Transcript turns (model output text, tool call args, tool results)
- Tool output payloads

All indexed as virtual table content in the session DB. Cheap to index, cheap to query. SQLite FTS5 ships with libsql; no new infrastructure.

## Why FTS5, not vectors

Vectors are tempting (turso has native vector indexes, F32_BLOB columns, vector_cosine), but they don't earn their complexity in v1.

Look at what recall queries actually look like:

- "Get step 5's full transcript" — structured lookup, no search needed
- "What files did I touch in steps 3–7?" — structured query
- "What was the exact error message we hit?" — lexical/FTS wins; vectors are bad at error strings, paths, identifiers
- "What did we already try for the auth bug?" — semantic, vectors help
- "What did I learn about the daemon socket layout?" — semantic, vectors help

Structured access + FTS5 covers the load-bearing cases with zero new infrastructure. Vectors require:

- Embedding model choice (OpenRouter doesn't host all embedders; might need a separate provider)
- Embedding cost per turn
- Index maintenance
- Dimensionality decisions
- Hybrid retrieval logic (vectors alone underperform on code/identifiers; you'd want FTS + vector with RRF or simple union)

The right time to add vectors is when specific query types are demonstrably underperforming on real usage. Add them then, not before. If/when added, they should target the prose-on-prose surfaces (findings, decisions, summaries) — not full transcripts (mixed code/prose, vectors are noisy on it).

## ID strategy

IDs are integers throughout — primary keys, foreign keys, ordering, joins. The agent references things in two ways:

1. **In tool calls** — by integer ID (`recall([3, 5])`)
2. **In prose** (summaries, findings, decision rationale) — by *semantic* reference, not ID. "The auth-token finding from the OAuth setup step" beats "see finding.marsh-pelican-trail." Semantic references carry meaning; ID references demand a lookup.

This was discussed at length in design. The case for anchor-word IDs (using `frances-anchors`) is real but conditional — it would only earn its keep if the agent were writing ID-shaped references in prose, and the better policy is to steer the agent toward semantic references instead. The conditions under which anchor-word IDs would become net positive are recorded in [open-questions.md](open-questions.md) so the option isn't lost.

The cross-session uniqueness argument for anchor-words doesn't carry: at promotion to the project DB, the session-scope row dies and the project DB row gets its own fresh PK. Numeric IDs are fine; namespacing with `<plan_id>:<step_id>` works wherever a globally unique handle is needed.

## Interaction with the gate

After a gate where the user picks `continue`:

1. Step N's status flips to `done` (or whatever outcome was claimed)
2. FTS indexes are updated with step N's content
3. Per-file summaries for files touched in step N are regenerated (see [file-summaries.md](file-summaries.md))
4. Step N+1's body is loaded; the next prompt is built using the hot/cold rules above
5. The model is invoked

After `discuss`, the same flow runs but with any structural changes from the discussion already applied. After `replan`, step N+1's body may differ from what was originally planned.

## What this replaces

The current binary has a single growing transcript inside the session DB and nothing reads it back into the model — once a turn falls off the in-memory window, it's gone for the model's purposes. The recall surface promotes the DB from "audit log" to "queryable working memory." That promotion is the architectural change; the rest is plumbing.
