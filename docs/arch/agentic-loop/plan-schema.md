# Plan and step schema

A plan is a typed structure, not a markdown file. The agent reads and writes it through structured tool calls; the TUI renders it natively at gates; the source of truth is rows in the per-session libsql DB.

## Plan

```
Plan {
  id,
  title,
  prelude: Prelude,
  steps: [Step],         // ordered, but steps have an explicit `seq` separate from id
  status: pending | active | completed | abandoned,
  created_at, updated_at,
}
```

The plan owns its steps — there is no plan-less step. A session can have multiple plans over its lifetime; one is "active" at a time.

## Prelude

The prelude carries the load-bearing context that doesn't decay. It is read at every step boundary. Distinct from per-step body, which is consumed and replaced.

```
Prelude {
  goal,                          // what we're trying to achieve, prose
  constraints: [Constraint],     // hard rules ("don't touch X", "must support Y")
  acceptance_criteria: [str],    // what counts as "the plan succeeded"
  references: [Reference],       // file paths to read, links, prior plan IDs
  glossary: [{term, defn}],      // domain-specific terms, optional
  notes: prose,                  // background context discovered during planning
}
```

Prelude is set when the plan is created and edited explicitly (separate tool call, surfaces in the gate). Steps inherit the prelude implicitly — no per-step copy.

## Step

```
Step {
  id,
  plan_id,
  seq,                           // ordering within the plan; separate from id
  title,                         // short, human-readable
  body,                          // what to do, context, instructions
  status: pending | in_progress | done | blocked | abandoned,
  outcome: succeeded | partial | failed | none,
  summary,                       // prose, written by agent at completion
  proof: Proof | null,
  findings: [Finding],
  decisions: [Decision],
  open_questions: [Finding],     // same shape as findings; questions vs answers
  artifacts: Artifacts,
  created_at, started_at, completed_at,
}
```

`status` and `outcome` are deliberately separate: a step can be `done` with `partial` outcome (e.g. "we did what we could; this part remains unsolved"). `abandoned` means "we decided not to do this" — recorded explicitly so replanning leaves an audit trail rather than silent deletion.

## Proof

Proof is the killer field. Without proof, "done" is hand-wave; with proof, the gate has something to actually inspect.

```
Proof = TestRun { command, exit_code, stdout, stderr }
      | BuildOutput { command, exit_code, output }
      | Diff { paths, summary }
      | UserConfirmation { message, turn_id }   // "user said: yes, that's what I meant"
      | SelfCheck { description, turn_ids }     // model re-read X and confirmed Y
      | None                                     // explicitly: no proof attempted
```

A step with `outcome: succeeded` and `proof: None` should be visually flagged at the gate — it means the agent claims success without evidence, which the user should examine.

## Finding and Decision

Findings and decisions have the same shape but different intent:

```
Finding {
  id,
  step_id,
  title,                         // short label ("auth-token-format")
  prose,                         // explanation
  evidence_turn_ids: [TurnId],   // back-links to the turns that produced this
}

Decision {
  id,
  step_id,
  title,                         // short label ("use turso-vector")
  prose,                         // rationale
  evidence_turn_ids: [TurnId],
}
```

- **Finding** = "we learned X" (factual observation about the code, the system, the user's intent)
- **Decision** = "we chose X because Y" (a choice that should not be relitigated)
- **Open question** = same shape as finding, but the prose is "we don't know X yet"

Findings, decisions, and open questions are addressable: future steps can reference them by title (semantic) or by id (precise). They survive compaction — the skeleton inlined into the next step's prompt includes finding/decision titles.

## Artifacts

```
Artifacts {
  files_touched: [{path, ops: [read | write | delete]}],
  commands_run: [{command, exit_code}],   // optional, for audit
  turn_ids: [TurnId],                     // every turn in this step
}
```

`files_touched` is the input to per-file summary generation (see [file-summaries.md](file-summaries.md)). `turn_ids` is the back-link from step → cold transcript storage.

## What's inlined vs cold

When the model is prompted at the start of step N+1, the inlined context is:

- The plan's full prelude
- For each completed step: `{title, summary, outcome, proof, findings, decisions, open_questions}` — *not* `body`, *not* full transcript
- The current step's `body`
- Any explicitly-recalled cold content from prior `recall` / `search` calls earlier in the current step

Everything else lives in the DB and must be pulled with `recall` or `search` (see [recall.md](recall.md)).

## Investigation steps

Some work is genuinely exploratory — debugging, "figure out how X works." The schema accommodates this without a separate type:

- `body` says "investigate X"
- `outcome` is `succeeded` / `partial` / `failed` based on whether understanding was reached
- `proof` is typically `SelfCheck` ("I re-read these files and now understand Y") or `UserConfirmation` ("user agreed the explanation matches their mental model")
- `findings` is where the actual content goes — that's the point of an investigation step
- `decisions` may be empty (investigation often doesn't decide things)

This is left as an open question: see [open-questions.md](open-questions.md) for the case for a separate `Investigation` step type with `findings: [Finding]` instead of `outcome+proof`.

## Splitting and replanning

A step that turns out to be too big should be splittable at a gate: agent proposes "this step needs to become 3 sub-steps," user approves at the gate, the original step's status becomes `abandoned` (or `done` if partial work landed), three new steps are inserted at its `seq` position. The original is preserved for audit.

A future step that was wrong should be rewritable at a gate: agent proposes "based on what we learned in step 3, steps 5 and 6 should change," user approves, the old steps are marked `abandoned` and replaced. Don't mutate steps in place — append + abandon, so the history reads clean.

## Storage

```
plans(id, title, prelude_json, status, created_at, updated_at)
plan_steps(id, plan_id, seq, title, body, status, outcome, summary,
           proof_json, started_at, completed_at)
plan_findings(id, step_id, kind, title, prose, evidence_turn_ids_json)
                                    -- kind: finding | open_question
plan_decisions(id, step_id, title, prose, evidence_turn_ids_json)
plan_artifacts(step_id, files_touched_json, commands_run_json, turn_ids_json)
```

IDs are integers. Foreign keys are integers. Agent prose uses semantic references (titles), not ID strings. See the ID strategy section in [recall.md](recall.md) for the reasoning.

FTS5 indexes are built over the agent-readable surface — see [recall.md](recall.md) for what gets indexed and why.
