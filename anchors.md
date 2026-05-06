# Line Anchor Design Notes

Working notes from designing a line-anchor system for LLM file edits, informed by reading dirac's existing implementation and discussion.

## Goal

Give the model stable per-line identifiers that survive edits, so it can target lines without using line numbers (which shift) or quoting full content (which is fragile and expensive). The model sees rendered files like:

```
Apple§def process(data):
Banana§    return data.strip()
```

…and emits edits that reference `Banana§` directly.

## Two layers: hashes vs anchors

There are two distinct things, both confusingly called "hashes" in casual conversation:

**Internal change detection** — FNV-1a 32-bit hash of each line's content. Stored as a `Uint32Array`. Used purely to detect which lines changed between snapshots. Never shown to the model.

**Visible anchor** — a word like `Apple` or `BananaCarrot`, separated from line content by `§` (or any clearly non-code delimiter). This is what the model sees and references.

The anchor is a *coordinate within a file*, like `foo.ts:42`. It must always be paired with the file path in the tool-call schema. Per-file uniqueness is enforced; cross-file collisions are fine and deliberate (cheaper anchors for small files).

## Two reconciliation paths

The system has to update anchors after two kinds of change, and they want different mechanisms:

**1. Our own edits (the common case).** When the model emits an `edit` tool call, we know exactly what changed. The anchor we're inserting before/after/replacing is named in the tool input. The lines being inserted are right there in the payload. Just transform the anchor array directly:

- `insert_after Banana§`: all existing anchors stay; mint fresh anchors for the inserted lines and splice them in at the right position.
- `insert_before Banana§`: same, but spliced before.
- `replace Banana§..Cherry§`: drop anchors for the replaced range; mint fresh anchors for the new lines.

No diff needed. No risk of hash collisions causing wrong matches. No CPU cost.

**2. External drift.** User edited the file in their editor, a formatter ran, `git checkout` happened — we don't know what changed. *Now* we run Myers diff:

1. Compute current FNV hashes for every line.
2. Fast path: hash array unchanged from cache → use cached anchors as-is.
3. Otherwise, Myers diff on the integer hash arrays. For each hunk:
   - **Unchanged** → carry over the exact old anchor.
   - **Removed** → drop.
   - **Added** → mint fresh from the pool.

Detection: compare current hash array against the cached one element-for-element on every read. If a difference is found, run the full diff. (Or use mtime as a cheap pre-check before hashing.)

A line's anchor is stable across edits as long as its content is unchanged — held by direct transforms for our own edits, and by Myers' equality matching when something external moved it.

## Word pool

Source: a precomputed dictionary of ~8200 short common English words, each chosen to be 1 token (or rarely 2) in the major BPE tokenizers. See "Building the dictionary" below.

Per-file state:
- `usedWords: Set<string>` — words already assigned in this file.
- `availablePool: string[]` — shuffled words not yet used. Pop from this.

When the single-word pool exhausts, fall back to **two-word compounds** (e.g. `BananaCarrot` or `banana-carrot`). With 8200 words that's ~67M unique two-word combos — effectively unlimited. Three-word compounds as an extreme fallback if needed.

Compounds are minted lazily, per-file, only when single-words run out. Always-compound would double the per-line token cost on every file render.

## Whitespace handling

**Hash trimmed content, not raw.** Whitespace-sensitive languages (Python, YAML, Haskell) don't change this — the *anchor's* job is "is this still the same line?", not "is the indentation correct?". A formatter reindenting a Python block leaves each line semantically the same; the anchor should survive.

**Inherit whitespace on edits.** Rule: if the model's edit payload starts with whitespace, respect it verbatim. Otherwise, inherit the leading whitespace from the anchored line. Insertions inherit from the anchor line. This eliminates a major class of LLM editing failures (miscounted spaces, dedent mistakes) and gives an explicit escape hatch when the model genuinely wants to dedent.

## Blank lines

All blank lines hash identically, which makes anchor assignment between adjacent blanks ambiguous. Fix: salt blank-line hashes with the nth-blank-in-file.

This works cleanly under Myers diff. Inserting or deleting a blank produces exactly one added/removed hunk; no cascade through the rest of the run. Walk through:

- Old: `A, B1, B2, B3, X` → hashes `[hA, s1, s2, s3, hX]`
- Insert blank between B1 and B2: `A, B1, Bnew, B2, B3, X` → hashes `[hA, s1, s2, s3, s4, hX]`
- Myers diff: common prefix `[hA, s1, s2, s3]`, added `s4`, common suffix `[hX]`. One new anchor minted; existing blank anchors preserved.

The "physical line" shuffle (the line originally called B2 now sits where B3 was) is invisible because blanks are *interchangeable*. The slot-positions of salted hashes in the recomputed array are stable: slot `k` always contains `salt(k)`. Myers compares hashes, not what physical line a hash "originally meant."

## Why per-file (not repo-wide) uniqueness

Considered making every anchor unique across the repo so the model could write `edit Apple§` without naming a file. Rejected:

- **Pool economics break.** A medium repo is 100k+ lines. With ~8200 single words, *every* anchor would be compound — doubles token cost on every line of every render.
- **State scope balloons.** A global `usedWords` Set across all files becomes a serialization concern; clean per-file isolation goes away.
- **Buys nothing the schema doesn't already give.** Tool calls have a `path` field. That's free disambiguation. Bare anchors without a path is *worse* tool design — you want the model to commit to a file explicitly.
- **Per-file collisions are a feature.** Small files keep cheap single-word anchors regardless of repo size.

Always require the file path. Anchors are file-relative coordinates.

## Why not OT (Operational Transform)

Rejected. The model emits anchor-relative edits like "edit `Banana§`" — that's already an OT-friendly addressing scheme without the OT machinery. We use direct anchor transforms for our own edits (see "Two reconciliation paths") and Myers for external drift. OT only earns its keep with concurrent writers needing convergence — we don't have that. The file on disk is authoritative; anchors are derived from it.

## Edit tool flow (round trip)

1. **Pre-edit**: load file. If the on-disk hash array matches our cached anchors' hash array, use cached anchors directly. If it diverged (external drift), Myers-reconcile to catch up. Render to model with anchors.
2. **Model emits edits** referencing those anchors. The `text` payload is plain content with no anchors. (Belt-and-braces: strip any anchors the model accidentally pastes back.)
3. **Apply edits**: splice new lines into the line array. Mint fresh anchors for inserted lines from the pool. Drop anchors for removed lines. Surviving lines keep theirs. Direct transform — no diff.
4. **Save**, then re-read post-save content (formatter may have run).
5. **Post-save reconcile (only if formatter changed things)**: compare hash array of saved content against our just-updated anchor array's hashes. If they match, done. If they differ, the formatter touched lines we wrote — Myers-reconcile against the post-save content to update anchors for the formatter's changes.
6. **Report back**: per edit, send context lines (with anchors), `-Anchor§` lines for each deletion (anchor only, no content — the content is already in the model's prior read of the file), and `+Anchor§content` lines for each insertion. No prose: the tool description in the system prompt teaches the format once; runtime output is pure data.

Three critical details:

- **Always reconcile *after* the formatter has run, not before.** Otherwise you hand the model anchors for line content that no longer exists on disk — the next edit's content-validation will fail.
- **Myers is for unknown changes only.** Don't run it for our own edits. Direct anchor transforms are correct, fast, and immune to hash-collision pathologies.
- **No explanatory prose in tool outputs.** The semantic contract (how to read diffs, how to use the new anchors, what auto-formatting means) goes in the tool description in the system prompt — said once, cached. Runtime outputs are pure data: anchors, sigils, line content. Repeating instructions on every edit burns tokens and clutters the model's view of what changed.

## Two design tricks worth stealing

**Diff-via-anchor-identity.** When rendering the post-edit diff block, compute "what was actually deleted" by set-differencing old anchor IDs against new anchor IDs in the affected range, rather than running a textual diff. Anchors *are* line identities, so set-difference on anchors == diff on lines. Free.

**Full-file fallback at high churn.** When the diff blocks would cover more than ~70% of the file, give up and re-print the whole file with all anchors. Cheaper in tokens than a sprawling diff and avoids forcing the model to stitch fragments. Reasonable threshold.

## Failure modes to design for

- **Model invents an anchor.** Fail loud with a clear "anchor X not found, did you mean Y/Z" message. Never silently fuzzy-match — that's how edits get applied to the wrong line.
- **Pool exhaustion mid-edit.** Fall back to compound words, not numeric IDs. Compounds remain readable.
- **Line content mismatch.** The model sometimes provides `Anchor§wrong-content`. Validate the content matches the actual line; reject if not. The model probably has stale state.
- **Formatter changes whitespace.** Anchors survive (we hash trimmed content), but the rendered line content updates. The diff block back to the model reflects the new content.

## Building the dictionary

The dictionary words must be (a) overwhelmingly 1 token in major BPE tokenizers, (b) common enough to be readable, (c) frequency-sorted so the cheapest anchors come first.

### Tokenizer coverage

Used `cl100k_base ∩ o200k_base` (OpenAI's BPEs from tiktoken) as a broad-compat proxy. Common-English-word vocabularies converge across modern BPE tokenizers, so this is a strong proxy for "1 token in most LLM tokenizers" without needing to run against every model.

Could extend with HuggingFace `tokenizers` for Llama-3, Gemma, Mistral. Marginal additional filtering. Skipped — the gains plateau fast.

Anthropic and Google tokenizers aren't publicly available; validate against their `count_tokens` APIs in a small sample if exact coverage matters.

### Casing matters

Tested three forms against tiktoken (cl100k + o200k), counting how many of 9884 source words are 1 token in each:

| Form        | 1-token | 2-token | 3-token | 4-token |
|-------------|---------|---------|---------|---------|
| ALLCAPS     | 1277    | 3925    | 3325    | 1150    |
| Title-Case  | 3832    | 5159    | 853     | 40      |
| lowercase   | 4151    | 4872    | 831     | 30      |

ALLCAPS is dramatically worse. BPE tokenizers see uppercase as relatively rare (acronyms, shouting), so all-caps forms tend to fragment.

But: **per-word case selection** beats single-form. For each word, try both Title-Case and ALLCAPS and pick whichever tokenizes cheaper. This wins for ~1100 words including genuine acronyms (`USA`, `BBC`, `NFL`, `III`) and a surprising tail of common short words that just happen to live in BPE vocab as ALLCAPS forms (`ABLE`, `AGO`, `LOSE`, `WAYS`).

The visual mix of `Apple` and `USA` in the dictionary is fine — the model handles both identically as anchors, the strip regex `\b[A-Z][a-zA-Z]*?§` accepts both.

Lowercase has the highest 1-token rate but doesn't satisfy the "anchor starts with capital" rule that makes anchors visually distinguishable from code content.

### Source list

Used `wordfreq` Python package: `top_n_list("en", 50000)` gives 50k frequency-sorted English words. Filtered to alphabetic-only, length ≥ 3 (1-2 letter anchors are visually weak and prone to confusion with content). 47,443 words after filtering.

Source list size matters. Aiming for 8000+ words from a 9884-word source forces low-quality 3-token survivors; from 47k it doesn't.

### Final result

8204 words, generated by:

```python
import tiktoken
from wordfreq import top_n_list

encs = [tiktoken.get_encoding(n) for n in ("cl100k_base", "o200k_base")]
words = [w for w in top_n_list("en", 50000) if w.isalpha() and len(w) >= 3]

def cost(w):
    return max(len(e.encode(w)) for e in encs)

def best_form(w):
    t, u = w.capitalize(), w.upper()
    ct, cu = cost(t), cost(u)
    return (t, ct) if ct <= cu else (u, cu)

annotated = [(*best_form(w), i) for i, w in enumerate(words)]
ranked = sorted(annotated, key=lambda t: (t[1], t[2]))
keep = ranked[:8204]

with open("words-anchors.txt", "w") as f:
    f.write("\n".join(t[0] for t in keep) + "\n")
```

Distribution: **4839 single-token + 3365 two-token, no 3-token survivors.**

Sort key is `(token_count_asc, original_frequency_index_asc)` — cheapest anchors first, with frequency as tiebreak so common words come before rare ones at the same token cost.

## Dictionary persistence and pool refill

State per file (in memory; serialize for cross-session continuity):

```
{
  hashes: Uint32Array,      // FNV per line, for diff
  anchors: string[],        // word per line
  usedWords: Set<string>,   // assigned words, for uniqueness
  availablePool: string[]   // shuffled, pop on demand
}
```

LRU eviction: cap tracked files per task and tracked tasks total. Memory per file is small (~kilobytes for typical files), but bound it anyway.

When `availablePool` exhausts: refill with random two-word combos drawn from the dictionary, dedup against `usedWords`, shuffle, push to pool. Fall back to three-word combos in extreme cases.

## Open questions / deferred

- **AST-level anchors.** A second layer that anchors *functions/classes*, not just lines. Survives reformats wholesale (line anchors for a function all reanchor on `cargo fmt`, but the function-level anchor stays put). Worth designing in early; line and symbol anchors compose.
- **Returning deleted anchors to the pool.** Currently a deleted line's word stays in `usedWords` forever. For long-lived tasks on large files with churn, you exhaust the pool faster than necessary. Cheap fix: return after N reconciliations to avoid confusion if the model still references the deleted anchor in flight.
- **Cross-session persistence.** Snapshot per-file state to disk so anchors survive process restarts.
