# Open questions

Things this design did not resolve. Recorded so they're not lost — to be answered when implementation makes the right answer obvious, or when real usage forces a decision.

## Investigation steps: same schema or distinct?

Debug-and-explore work doesn't fit `outcome + proof` cleanly. The current design says: an investigation step is a regular step with `body: "investigate X"`, `outcome: succeeded` if understanding was reached, `proof: SelfCheck` or `UserConfirmation`, and `findings` carrying the actual content.

The alternative is a dedicated `Investigation` step type with `findings: [Finding]` instead of `outcome + proof`. Cleaner mental model for exploratory work, but adds a type variant that other parts of the system have to handle.

Probably the regular-step-with-loose-proof approach is fine. The risk is degenerating into unstructured agentic stew where every step claims to be "investigation" to dodge the proof requirement. Worth watching once layer 1 is in use.

## "Discuss" state mechanics

The discuss state is supposed to make user feedback land as durable artifacts (findings, decisions, edits to future steps, stronger proof on the just-completed step) rather than ephemeral chat. But the exact mechanism is not specified:

- How does the agent decide that user feedback warrants a structural change vs a one-time clarification?
- What's the prompt shape that biases toward producing structured changes?
- Should there be an explicit "land this as a finding" tool the agent reaches for during discussion?
- What if the user's feedback is purely tonal ("sounds good but be more careful next time") — does that map to anything durable?

Likely needs prompt experimentation in real use. Don't over-design upfront.

## Project identity stability

The design proposes canonical absolute path of the working directory as the project key, with an optional `.frances/project-id` override file. Open:

- Should the override file be auto-created on first session, or only on user request?
- What happens when a project is cloned to a new path? (Two project DBs with the same content, no link between them.)
- Git remote URL as a fallback identifier — useful or noise?
- For monorepos where multiple "projects" might live in subdirectories — is the project identity always the cwd, or is there a discovery mechanism (walk up looking for `.frances/`)?

Not solving this until project DB is being built and real usage shows what breaks.

## Reads vs writes in per-file change log

Does reading a file (without editing) produce a `file_change_entries` row?

- **Argument no:** the file didn't change. Reads belong in the step's `findings` ("I read `src/llm.rs` and noticed X"), not in the file's evolution log.
- **Argument yes:** "what files has this plan touched at all" is a useful query, and reads are part of touching a file. Could be a separate column or a separate kind.

Lean: no — reads in findings, writes in change_entries. But this is reversible if usage shows the read-tracking would be useful.

## Turn definition exactness

"Each LLM round-trip = one turn within its step." Open:

- Is a turn one model invocation (one streamed completion), or one logical request-response unit?
- How are tool calls within a turn counted? One model invocation can produce multiple tool calls; each tool call has its own result. Does the agent see all of them as part of the same "turn" or separate turns?
- For purposes of `recall(step_ids, { transcript: { from: 3, to: 7 } })`, what does turn 3 mean if the model produced 5 tool calls in its third invocation?

Probably: turn = one model invocation, producing N tool calls and 1 model output. Tool call results are *attached to* a turn but not themselves turns. Worth nailing down in the schema doc once the messages table is being designed.

## Conflict resolution on parallel sessions

Two TTYs both edit `src/llm.rs` in different sessions, both promote to project DB. The current answer is "don't merge, store both with session_id annotation." Open:

- Does the agent in a third session need any UI/tool support for distinguishing "the latest" from "the conflicting"?
- Is there ever a case where automatic merge is reasonable? (Probably not; let humans resolve.)
- Should the project DB surface a warning when reading conflicting summaries?

Wait until two-parallel-sessions-on-same-project is actually a thing people do.

## Project DB GC

Project DBs grow forever. Out of scope for v1 but eventually needs:

- By-age (drop everything older than N days)
- By-status (drop plans with status `abandoned` after some grace period)
- By-relevance (drop findings about files that no longer exist)
- User-driven (a UI command "clean up project memory")

Probably solved by waiting for a real complaint, then picking the cheapest mitigation that addresses it.

## Anchor-word IDs revisited

The current design uses numeric IDs throughout and steers the agent toward semantic references in prose. The anchor-word ID alternative was considered and rejected for "purely internal" use.

The conditions under which anchor-word IDs would become net positive:

- **If** the agent starts writing ID-shaped references in prose (e.g. "see finding #marsh-pelican" rather than "see the auth-token finding")
- **And** those ID references appear in user-visible content (gate summaries, plan rendering)
- **And** lexical robustness (hard-to-confuse identifiers, FTS-precision, fabrication resistance) becomes load-bearing

If those conditions hold, anchor-words are worth revisiting — for findings, decisions, and other agent-prose-referenced entities. Stay flexible: numeric PKs in the schema today don't preclude adding an anchor-word handle column later.

## Cost ceiling for summarizer pass

The summarizer pass at step completion runs N parallel calls (N = files touched). On a wide step (many files) this can spike. Open:

- Is there a max-files-per-step limit before the summarizer falls back to batched/sequential mode?
- Should the summarizer skip files with trivial diffs (e.g. <10 lines changed) to save cost?
- Does the user get a budget signal? ("This step touched 47 files; summarizer pass would run 47 calls")

Probably not a real problem until someone hits it. Premature.

## Patch proposal pass shape

Plan patches are proposed by a main-model pass after summarization (see [gate.md](gate.md) — "Patch proposal pass"). Open:

- What patch types are allowed vs forbidden? Renames clearly yes, body rewrites clearly no, but the middle (e.g. "add a sentence about a constraint we discovered") is fuzzy.
- Should the patch pass output confidence per patch so the gate can auto-apply high-confidence ones in full-auto mode while flagging uncertain ones?
- Precedence when patches and replan collide on the same step: probably "user always wins" but the patch metadata should be preserved as part of the audit trail.
- Cost ceiling: a main-model call at every step boundary is real money. Worth measuring whether the conditional skip ("no upcoming steps to patch") catches enough cases or whether further heuristics are needed.

Practical question; resolve when the patch proposal pass is being implemented.

## What "plan complete" actually means

Promotion to project DB triggers on "plan completion." But what makes a plan complete?

- All steps have `status: done`?
- User explicitly marks it complete at a final gate?
- All `acceptance_criteria` from the prelude are checked off?

Probably user explicitly marks complete, possibly with a checklist of acceptance criteria. Not yet specified.

## When should the agent reach for `recall` vs `search`?

The two-tool surface is good but the agent needs a learned policy on which to use when. This is more a prompt-design problem than a schema problem, but worth noting:

- Cold-start case: agent doesn't know step IDs of relevance, must `search` first
- Warm case: agent already knows step IDs from skeleton, can `recall` directly
- Pathological case: agent over-recalls, blowing context budget

Tune via system prompt and observation of real behavior. Worth a section in the implementation doc once written.
