# User gate

The user gate is the checkpoint at the boundary between two steps. It is what makes mandated structured planning bearable: the user stays in the loop without micromanaging, and the agent commits to "I claim this is done" with proof before moving on.

## Trigger

A gate fires when the agent emits a structured task-completion event for the current step. The event carries:

- `outcome` (succeeded / partial / failed)
- `summary` (prose, what happened)
- `proof` (test run, build output, diff, self-check, user confirmation, or explicitly none)
- `findings`, `decisions`, `open_questions` (final values for this step)
- `artifacts` (files touched, turns)

Without this event, the step is not done and no gate fires. This is deliberate — it forces the agent to commit explicitly rather than drift to the next thing.

## What the user sees

A typical gate render:

```
┌─ step 3: wire OAuth token refresh ─────────────── done · succeeded ─┐
│                                                                      │
│  Summary:                                                            │
│    Added refresh_token() to AuthClient; calls /auth/refresh on 401   │
│    and retries the original request once. Plumbed through            │
│    SessionState so concurrent calls share a single in-flight refresh.│
│                                                                      │
│  Proof:                                                              │
│    cargo test -p frances auth::tests — passed (3/3, 0.4s)            │
│                                                                      │
│  Findings:                                                           │
│    • token-clock-skew — server allows 30s skew; we don't preemptively│
│      refresh. Acceptable for now.                                    │
│                                                                      │
│  Decisions:                                                          │
│    • single-flight-via-mutex — chose tokio::Mutex over a channel     │
│      because the call is rare; channel was overkill.                 │
│                                                                      │
│  Open questions:                                                     │
│    • What happens if /auth/refresh itself returns 401? (next step)   │
│                                                                      │
│  Files: src/auth.rs, src/session.rs                                  │
│                                                                      │
│  Up next, step 4: handle refresh-of-refresh failure                  │
│                                                                      │
└── [c] continue   [d] discuss   [r] replan   [a] abandon ─────────────┘
```

The four actions:

- **continue** — the next step's body is loaded and the agent starts. Default.
- **discuss** — see below; opens a structured discussion state.
- **replan** — opens the plan editor with all pending steps editable. Agent gets a chance to propose changes given what was just learned; user accepts/edits/rejects.
- **abandon** — the plan is marked abandoned. Useful when "this whole approach was wrong" is clearer than fixing individual steps.

## The discuss state

Discuss is not free-form chat — it's a structured state that makes user feedback land somewhere durable. When the user picks discuss:

1. The UI opens a chat overlay scoped to the just-completed step's context. The agent has access to the step's full transcript, not just the skeleton.
2. The user types feedback, asks questions, points out concerns.
3. The agent responds. If the user's feedback implies a structural change — "actually, the proof is too weak, run the integration tests too" or "step 5 should now do X instead" — the agent proposes a structured change (re-run with stronger proof, edit a future step, add a finding, etc).
4. Each proposed change is shown for approval.
5. When the discussion concludes, the user picks one of: continue / replan / abandon.

The structural part matters. A free-form back-and-forth that ends with "okay continue" risks dropping all the user's feedback the moment compaction happens at the next gate. By forcing feedback to land as findings, decisions, edits to future steps, or stronger proof on the just-completed step, the discussion produces durable artifacts that survive into the next step.

This is left as an open question: see [open-questions.md](open-questions.md) for unresolved details on exactly how the agent decides when feedback warrants a structural change vs a one-time clarification.

## Patch proposal pass

Between summarization and the gate, a separate pass invokes the *main* model to propose patches to upcoming steps. The flow:

1. Step completes; agent emits outcome event.
2. Cheap summarizer pass runs (see [file-summaries.md](file-summaries.md)) — produces per-file summaries and any prose summary refinements.
3. **Main-model patch proposal pass.** Inputs: the cheap summarizer's output, the just-completed step's findings/decisions, and the upcoming-step bodies. The main model is asked: "given what was just decided/built/named, do any upcoming steps need patching?" Output: structured patches, or "no patches needed."
4. Gate fires, showing the step summary + any proposed patches.

The two-step structure (cheap-summarize, then main-model-decide) keeps each model's job at its right size: the summarizer compresses information; the main model makes plan-edit decisions. It also means that when the user picks `discuss` at the gate, they're discussing patches *with the model that proposed them* — the same model running the main loop. That's the conversation the user actually wants.

Patches are mechanical in nature — rename propagation, decision insertion, vocabulary alignment — not structural rewrites. Substantive plan changes (new requirements, reordering, splitting) still go through `replan`, which gives the main model an explicit "rewrite the rest of the plan" prompt rather than the targeted "patch where needed" prompt.

```
  Proposed plan patches:
    ▸ step 7: rename `TokenCache` → `AuthCache` in body
    ▸ step 5: add note "uses tokio::Mutex (decided in step 3)"
    [enter] approve all  [e] edit individually  [r] reject all  [d] discuss
```

User options:

- **Approve all** — patches apply, original step bodies kept as prior versions.
- **Edit individually** — open a patch-by-patch view; accept, reject, or modify each.
- **Reject all** — no changes.
- **Discuss** — enter the discuss state with the main model. Ask "why this rename?", push back ("don't rename, keep `TokenCache` for compat reasons"), or expand scope ("also patch step 9 while you're at it"). Outcome of the discussion is a revised patch set the user then approves.

In full-auto mode, the patch proposal pass still runs, patches are auto-applied if proposed, and they're visible in the step's post-state for audit. Same exception rules as the gate itself: if any patch is flagged uncertain by the main model, surface a gate.

**When the patch pass can be skipped.** If there are no upcoming pending steps (last step) or the just-completed step touched no files and made no decisions, skip the pass — there's nothing to propagate. This avoids paying for a main-model call that will trivially output "no patches."

**Audit trail.** Original step bodies are preserved; patched versions are new revisions. Replan-after-patch and patch-after-replan are both possible; the history records the order. If the user approves a patch and later regrets it, reverting to the prior body is trivial.

## Referee

A model with the `referee` intent determines if the work and proof of work are satisfactory, if not,
the main model is instructed to do the work correctly.

## Full-auto mode

Full-auto suppresses the gate UI but does not change the underlying structure:

- The agent still emits step-completion events
- Outcomes, proofs, findings, decisions are still recorded
- The plan still progresses one step at a time with state persisted
- The user can return at any time to see what happened

The difference is only that there is no human pause between steps. The model the user has the option to return to is "I left this running; what did it do?" — and because the plan structure is intact, that question has a real answer: open the plan, scroll the steps, look at proofs.

Full-auto should still surface a gate when:

- Outcome is `failed` (the model claims it couldn't do the step)
- Proof is `None` and outcome is `succeeded` (claim without evidence)
- Agent itself signals uncertainty (a special "request gate" outcome flag)

These exceptions exist because full-auto's promise is "trust me when things are going well, ask when they're not." Mechanically silencing every gate would defeat the structure's purpose.

## What is *not* a gate

- Mid-step pauses for the user to clarify ambiguity. Those are tool calls (the model asks a question, the user answers, the model continues — all within the current step).
- Tool authorization prompts. Those are orthogonal — they live in the session runtime's permission layer, not the plan layer.
- Compaction of the agent's context. That happens transparently at every step boundary regardless of gate vs full-auto. The gate is about user steering; compaction is about prompt management.

## Implementation hooks

The UI needs:
- A renderer for the step skeleton + proof
- The four-action keymap
- The discuss overlay
- Plan editor for replan

The session runtime needs:
- An event variant for "step completed, here's the event"
- A method for "user chose action X" (and any structural changes from discuss)
- Persistence of all of the above through the existing per-session DB

See [recall.md](recall.md) for how the post-gate state feeds the next step's prompt.
