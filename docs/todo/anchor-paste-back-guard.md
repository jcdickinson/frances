# Guard against the model pasting anchors into edit payloads

## The failure

Models sometimes put anchor-shaped text into the `text` payload of an
insert/replace, and it ends up written into the file verbatim. Two modes,
observed in the wild:

- **Copy-back.** The model reproduces the rendered lines it just saw
  (`Apple§def foo():`) instead of bare content, prefixes and all.
- **Invented anchors.** The model mints *fresh* anchor words for the new lines
  — it has learned "new lines get anchors" and helpfully pre-supplies them.
  These words are **not** in the file's live anchor set.

Root cause: within a session the model sees `anchor§content` far more than it
sees bare content (every read render, every diff echo), so its prior for "what
file content looks like" gets polluted. Instructions not to do it help but
don't eliminate it.

## Why not just strip

A blanket strip of anything matching `[A-Z]\w*§` corrupts legitimate content —
editing the anchor engine itself, test fixtures, prose with a capitalized word
before `§`. The model must retain the ability to insert anchor-shaped text when
it genuinely needs to. So: detect and **reject loud**, with an explicit
override, rather than silently mutate the payload.

## The detector

Fire only when **every non-blank line** of the payload matches
`^[A-Z][\w-]*§` (and there is at least one such line).

Rationale: the hallucination is a *mode switch* — the model renders the whole
block in display format, uniformly. A partially-prefixed payload is therefore
evidence *against* paste-back (more likely real content with a coincidental
`Word§` line). The all-non-blank-lines gate is both simpler and more precise
than a consecutive-run heuristic, and it naturally covers the single-line
insert. Blank lines are excluded from the quantifier so blank separators in
code blocks don't defeat it.

This is structural, not membership-based — it catches invented anchors too,
which a `usedWords` check would miss. (Keep live-set membership only as a
*wording* signal: "these match current anchors" vs "these look invented".)

Depends on every anchor being **capital-initial**, which holds for the current
`words.txt` (all 2058 entries are Title-case; the generator only does
`.capitalize()`). If lowercase words are ever added to the dictionary to grow
the 1-token population, this regex must widen or it'll miss lowercase-prefixed
paste-backs.

Known gap, not worth building for yet: a *mixed* paste-back (model echoes a
couple of real context lines, anchored, then appends new unanchored content)
slips through. Rarer than the uniform case; the membership check is a cheap
second net if it ever shows up.

## The override: `bypassAnchorGuard`

Per-edit-op boolean, default false/absent. When the detector fires and the flag
is unset, reject. When set, the payload is written verbatim.

The flag is what makes an aggressive high-recall detector safe: a false positive
is recoverable by the model in one step, so we don't need high precision. High
recall + cheap recovery beats high precision + silent miss. Same shape as the
existing leading-whitespace escape hatch (see `anchors.md` "Whitespace
handling").

**Name and schema description matter.** The model sees the param in the schema
on every call, so it must not invite preemptive opt-in. `bypassAnchorGuard`
(not `allowAnchors`) frames it as overriding a protection — an affordance models
are reluctant to reach for — and references a mechanism the model only learns
about from the runtime rejection. Keep the schema description minimal — the
rejection message carries the full explanation, so the schema only needs to
deter preemptive use and point at the error:

> Leave unset. If you ever need it, a rejection from this tool will say so.

Keep one property in the implementation: the flag is only consulted inside the
detector branch, so setting it speculatively gains the model nothing. No upside
to preemptive use beats a prohibition against it.

## Error wording is load-bearing

The risk: the model reads "add `allowAnchors`" as an instruction to toggle the
flag and resubmit the same garbage, bypassing the guard. The message must bias
toward **removing** the prefixes and frame the flag as a rare exception:

> Your insert text has lines beginning with anchor tokens (`Apple§`,
> `Banana§`). Anchors are assigned automatically — your `text` should be the
> bare line content with no anchor prefixes. **Remove them and resubmit.** Only
> if you genuinely intend these exact characters as literal file content,
> resubmit with `allowAnchors: true`.

- Imperative is "remove them"; the flag is conditional.
- **Echo the offending lines** so the model sees its own mistake concretely.
- Tool description: never set `allowAnchors` unless literal anchor-shaped text
  is specifically needed — keeps it the cold path.

## Probability reduction (separate from the guard)

The guard is the backstop; also reduce how often it triggers:

- **Contrastive example in the tool description.** A demonstrated negative
  (`WRONG → text: "Apple§foo()"` next to `RIGHT → text: "foo()"`) beats a
  prohibition. It lives in the cached tools block, so it's near-free per turn.
- **Keep runtime output minimal** (already the convention, `anchors.md` step 6).
  Every extra `Anchor§content` line echoed back reinforces the paste-back
  pattern within-session — the "pure data, no prose" discipline is also error
  reduction, not just token economy.
