# Prompt sections, tool families, and baseline context

Build prompt assembly in JS, give the model the situational context it currently
lacks, and stop duplicating shared guidance across tool descriptions. Three
changes, in dependency order.

## Motivation

There is no system-prompt assembly today. Messages are forged per input type in
`crates/frances-llm/src/providers/genai/mod.rs` (`forge_one`); a
`HistoryInput::System { text }` becomes a `ChatMessage::system(text)`
(genai/mod.rs:334) pushed via `chat.push()`. Nothing injects the working
directory, OS, shell, or git state. `WorkflowDeps::current_cwd()` /
`current_env()` exist (`crates/frances-session/src/runtime/mod.rs:115-122`) but
never reach the model.

Observed symptom: the model prepends `cd /home/user/ && …` to nearly every
`shell_run` call. `/home/user` is a stock training-data placeholder, not a path
it observed — it doesn't know where it is. And it re-anchors *every* command,
which is what you do when the shell is stateless. Our shell is persistent
(`shell.js`: `Run`/`Wait`/`Kill`/`Set` share state), so the model is defending
against the absence of a feature we have. Two knowledge gaps, same cause: **the
model isn't told the cwd, and isn't told the cwd persists.**

Separately, the edit-tool descriptions duplicate a large block of prose. See
`crates/frances-workflow/src/modules/desc/file_replace_lines.md`,
`file_insert_after.md`, `file_insert_before.md` (and the `file_new` /
`file_overwrite` precondition line): the `CRITICAL: text must NEVER contain a
Word§ prefix` block with its WRONG/RIGHT example, the `Anchor protocol:`
paragraph, the `Must have been read this turn via file_read` precondition, the
`Provide exactly one of text or from` exclusivity, and the post-edit formatter
note are near-verbatim across tools. That shared guidance wants one home.

## Change 1 — prompt assembly in JS (the foundation)

Nothing else can land until the prompt is assembled in JS. This change builds the
machinery and makes it the *only* path to the system prompt.

`ChatSession` (`crates/frances-workflow/src/modules/js/chat.js`) is barebones
today: a mutable `tools` array, `push`, `stream`. Give it an ordered list of
**sections**.

A **section** is a stable `{ prompt(ctx) -> string | null }` value (null = "I
have nothing to emit; skip me, don't even print a separator"). Sections must be
**stable references** — defined once and referenced — or the `===` dedupe (below)
can't fire. Not inline closures rebuilt each assembly.

```js
session.promptSections = [envBlock, toolGuidance, globalAgents, localAgents, nestedAgentsInventory];
```

(`globalAgents` / `localAgents` are the instruction-discovery sections specified
below; `localAgents` sits after `globalAgents` so the project overrides the
user/system scope.)

### Remove system pushes from the JS interface (keep the Rust role)

`HistoryInput::System` (genai/mod.rs:334) stays — assembled sections have to
become a system message on the wire, so the Rust-side role is exactly how their
output reaches the model. What's removed is the *JS affordance*: reject
`role: "system"` pushes in `chat.push()`. Sections become the only JS path to
system-prompt content, and ChatSession renders their concatenated output into the
single Rust `System` message at build time.

A workflow that wants a fixed system string registers a section that returns that
constant — no parallel mechanism. This makes the two-JS-paths invalid state
unrepresentable, and because sections are recomputed at each build the system
prompt stays live (current cwd, current git state) instead of being frozen into
persisted history.

### Modes compose sections; sections stay mode-agnostic

Different parts of the workflow expose different tools (referee, auto mode, …). A
mode is, in effect, an ordered list of sections. **Inclusion = presence in the
list.** Auto mode lists `agentsMd`; referee simply omits it.

The exclusion lives in the *mode definition*, never inside the section. A section
must not contain `if (mode === "auto")` — that scatters mode-knowledge and
reintroduces drift. Sections answer only "given ctx, here's my text or null." Two
gates compose at the right layers:

- **Mode gate (policy):** is this section eligible at all? → its presence in the
  list.
- **Section gate (data):** even when eligible, do I have anything? →
  `localAgents` returns null when no instruction file exists on disk.

The assembler iterates the mode's explicit ordered list, so ordering stays
authored (legible, deterministic for caching) — not a global registry with
priority fields.

### ctx is read-only data; capabilities are imports

Sections receive a `ctx`. **Do not pass `ChatSession` itself into ctx** — that
hands every section a god-object handle it can mutate and reach unrelated state
through, and you can no longer tell what a section depends on or test it without
a whole session.

- **Data → ctx:** stable env (OS, shell, repo root, cwd snapshot) *plus the
  available tool set* (`ctx.tools`, used by `toolGuidance` in Change 3). Unused
  fields in a shared data bag are cheap; a live mutable handle is not.
- **Capabilities → imported modules:** instruction discovery is the example. JS
  has no filesystem access and shouldn't. A Rust-backed module exports
  `discoverGlobalAgents(ctx)` / `discoverLocalAgents(ctx)` that cross into Rust,
  do the XDG/dirs traversal (reuse the machinery in `runtime/mod.rs:434-473`,
  `xdg::BaseDirectories::with_prefix("frances")`), and return content-or-null.
  The `globalAgents` / `localAgents` sections import and call them — never
  `ctx.fs`. Algorithm specified under "Instruction discovery" below.

## Change 2 — baseline context (fixes the cd bug)

With the section machinery in place, author the context sections. This is the
bug fix: tell the model where it is and that the shell persists.

### Shell guidance (kills the `cd /home/user` behavior)

State this near the shell tools (its permanent home is the `shell` family in
Change 3, but the content is what matters):

- The shell is **persistent**: the working directory and environment persist
  across `shell_run` calls.
- You are already in the working directory shown below. **Do not** prefix
  commands with `cd` to an absolute path.
- To change directory for subsequent commands, run `cd <dir>` as its own command;
  it persists.
- Use paths relative to the working directory, or absolute paths.
- Prefer the dedicated tools over shell equivalents: `file_read` instead of
  `cat`/`head`/`tail`, `file_find_or_grep` instead of shell `grep`/`find`. Use
  `shell_run` for actually running programs.

(That last line is the same class of guidance a good harness gives — "don't
shell out for what a first-class tool already does." It also steers the model
off habits that produce worse results than our anchored tools.)

### Environment block

- **Working directory** — the real absolute cwd.
- **OS, shell, platform** — so it doesn't emit shell-isms for the wrong shell.
- **Date** — so it doesn't guess "now".
- **Git snapshot** — branch, status, recent commits, flagged as a point-in-time
  snapshot that won't update.

### Cache-prefix rule

Tool definitions serialize near the front of the request (the cached prefix).
**Do not put mutable state in the cached prefix.** cwd is mutable — the
persistent shell lets the model `cd` and have it stick — so a live cwd embedded
in a front block that regenerates each turn busts the cache on every `cd`, and
everything downstream with it.

Split context by mutability:

- **Immutable for the session** (OS, shell, repo root, the persistent-shell
  rule): fine up front, cache-stable.
- **Mutable** (live cwd): emit in a *late* section each turn, where re-rendering
  costs nothing. An initial-cwd snapshot up front is fine; the *live* value
  belongs late.

## Change 3 — `ToolFamily` + dedupe descriptions

A `ToolFamily` is a shared, context-aware preamble that a set of tools points at.
Its only job right now is to carry one prompt function, so it is deliberately
tiny:

```js
const editing = defineToolFamily({
  prompt: (ctx) => `…the anchor protocol, the Word§ CRITICAL warning + WRONG/RIGHT
                    example, the "read this turn" precondition, text/from
                    exclusivity, the post-edit formatter note — ONCE…`,
});
const shell = defineToolFamily({ prompt: (ctx) => /* the Change-2 shell guidance */ });
```

- It is a **fn, not a prop**, because it takes `ctx` and is computed once at build
  time. A static string can't hold runtime values.
- It is an **object referenced by identity**, not a string tag. `===` dedupe is
  the mechanism (below); a `group: "editing"` string would be the stringly-typed
  smell this codebase rejects everywhere else.

Tools point at their family; **membership is one-way (tool → family)**. The
family never lists its members — the tool is the unit of availability, so
"which families are present" is derived from "which tools are available". This
keeps the family at exactly `{ prompt }` and nothing else (no members array to
rot).

```js
defineTool({ name: "file_replace_lines", family: editing, description: /* thin */ });
defineTool({ name: "file_insert_after",  family: editing, description: /* thin */ });
```

Each per-tool description shrinks to **what is unique to that tool**: its
one-line purpose, its args, its worked example. The anchor-protocol boilerplate
moves into `editing.prompt` and is emitted once; the shell guidance from Change 2
consolidates into `shell.prompt`.

**Dedupe-by-identity gives "compute once" for free.** The assembler unions
families over the available tools — `new Set(tools.map(t => t.family).filter(Boolean))` —
and calls each present family's `prompt(ctx)` exactly once. A mode that exposes
no edit tools (referee) never lands `editing` in the set, so its preamble never
enters that prompt.

### `toolGuidance` and the single tool list

Disentangle two things hiding under "tools":

- **Tool schemas** (the JSON defs) are not prompt text. They ride OpenRouter's
  separate `tools` API field — `tool_def_to_genai` / `req.tools = …` in
  `providers/genai/mod.rs:357,373-380`, parsed from `chat.tools` by
  `snapshot_tools` / `parse_tool_defs`
  (`crates/frances-workflow/src/modules/chat.rs:641-692`) into
  `ToolFunction { name, description, parameters }`
  (`crates/frances-models-llm/src/tool.rs`). **Not** a `promptSection`.
- **Tool guidance** (the family-union prose) *is* system-prompt text and *is* a
  section — `toolGuidance`.

The deduped tool list is the **single source of truth**, held in JS. ChatSession
derives both consumers from that one list: the wire `tools` field (serialize each
schema) and `ctx.tools` (which `toolGuidance` folds families out of). Same list
feeds both, so guidance and schemas **can't drift** — you can't advertise a tool
in the prose that isn't in the `tools` field or vice versa.

`toolGuidance` does the family fold itself, from `ctx.tools`:

```js
const toolGuidance = { name: "tool-guidance", prompt: (ctx) =>
  foldFamilies(ctx.tools)   // union by ===, call each present family.prompt(ctx), join; null if none
};
```

So **ChatSession never references `ToolFamily`** — it only knows "here's the
deduped tool list; serialize schemas from it and put it in ctx." Families stay
entirely inside the one section that renders them.

`toolGuidance` is a stable section the *author positions* in `promptSections`,
not something ChatSession silently appends at the end. The author controls
*where* it lands; ChatSession controls *what's in it*. It returns null when no
families are present.

### Why `ToolFamily` does NOT implement a shared `Prompted` trait

Considered and rejected for now. A family is **not** a top-level prompt section
and is not in `promptSections`. It is consumed *by* `toolGuidance`, deduped in a
different scope (families over the tool set vs. sections over the prompt). Family
`prompt(ctx)` and section `prompt(ctx)` share a *shape* but live at two layers
with different gating (tool-present vs. file-exists vs. mode-listed) and
different ctx needs; flattening them into one trait/registry would force all that
through one signature. Add a shared interface only when a second *real*
implementer appears and the assembler is visibly copy-pasting the same "call it,
skip if empty, concat" dance — not for an audience of one plus a hypothetical.

## Instruction discovery — `globalAgents` and `localAgents`

Two sections, each backed by a Rust capability (`discoverGlobalAgents` /
`discoverLocalAgents`); JS has no FS. Both emit their sources **lowest-priority
first, highest-priority last** — the tail of the prompt is closest to generation,
where the model weights instructions most. `localAgents` is placed after
`globalAgents` in `promptSections` for the same reason: project beats user/system.

Position sets the *direction* of precedence but is not a hard override, so each
emitted block is labelled and the section prepends an explicit precedence note
("project instructions take precedence over global; `.local` over shared"). For a
genuine conflict the explicit statement is what makes it deterministic; position
only nudges.

### Global (`discoverGlobalAgents`) — lowest → highest

1. `~/.claude/CLAUDE.md` — interop with the user's existing Claude config.
   Lowest. (Deliberate: frances picks up the user's Claude instructions.)
2. `XDG_CONFIG_DIRS` (system; colon-list, earlier entries rank higher per XDG):
   `AGENTS.md`, then `frances/AGENTS.md`.
3. `XDG_CONFIG_HOME` (user; outranks all system dirs): `AGENTS.md`, then
   `frances/AGENTS.md`.
4. `$HOME/AGENTS.md` — highest.

Within any location the generic `AGENTS.md` ranks **below** the frances-specific
`frances/AGENTS.md` (more specific wins → generic first, frances last). This is
the one correction from the original sketch, whose XDG steps had them reversed;
Local already had it right.

### Local (`discoverLocalAgents`), rooted at the first editable root — lowest → highest

1. `root/CLAUDE.md`, then `root/CLAUDE.local.md`
2. `root/AGENTS.md`, then `root/AGENTS.local.md`
3. `root/.agents/frances/AGENTS.md`, then `root/.agents/frances/AGENTS.local.md`

`.local.md` ranks above its shared sibling (personal override); `.agents/frances/`
is most specific (highest). Specificity increases down the list.

`root` is the first entry of `WorkflowDeps::editable_roots()`
(`crates/frances-workflow/src/deps.rs:47`) — already computed once by design and
used by the shell/file modules; **do not** recompute via `discover_editable_root`.
If the slice is empty there is no local scope and the section returns null.

Discovery is root-only: no per-directory `AGENTS.md` collected at intermediate
levels or in subdirectories. The AGENTS.md spec allows nesting, so this is a
deliberate deferral — `log()` if a nested file is ever silently skipped rather
than dropping it quietly.

### Dedupe (per scope)

1. Canonicalize every candidate path (resolve symlinks); drop duplicate canonical
   paths *before reading*, so a symlinked alias isn't read twice.
2. Read survivors; dedupe by content hash, keeping the first (lowest-priority)
   occurrence — collapses a `cp`'d or cross-symlinked CLAUDE.md/AGENTS.md.

Dedupe is per-scope: a `~/AGENTS.md` and a `root/AGENTS.md` are legitimately
distinct and both survive.

### Caching

Stable for the session (unlike live cwd), so these sit in the cacheable region.
Sections recompute each build, so a mid-session edit is picked up and busts the
cache only then.

### Nested awareness — the `nestedAgentsInventory` section

Local discovery is root-only, so the agent is otherwise blind to
`crates/foo/AGENTS.md` and the like. Close that gap with an **inventory** of
nested instruction files — not eager content load (the token cost we rejected),
and not a CRUD/event feed.

A CRUD feed is the wrong mechanism: most nested files already exist at session
start (no create/update/delete event ever fires for them), so a change stream
misses the dominant case. Awareness needs a listing. And freshness is already
handled — sections recompute each build, so an inventory that re-globs every turn
gets all of create/read/update/delete for free: pre-existing files listed from
turn 1, a newly created one appears next build, a deleted one drops out, no
watcher or event plumbing. A turn-based agent never acts mid-turn, so there is
nothing a push notification would add.

Shape:

- **Paths, not content.** List the nested `AGENTS.md` paths (a dozen paths is
  cheap; a dozen files is not). Exclude anything `localAgents` already injects —
  this section is *only* the nested files. Content is pulled on demand.
- **Nudge to read.** The block instructs: local instruction files exist at these
  paths; read the nearest before working in that subtree.
- **Walk must see untracked files.** Use a filesystem walk with ignore rules
  (skip `.git`, `target`, `node_modules`; respect `.gitignore`) rather than
  `git ls-files`, so a freshly-created `AGENTS.md` the agent hasn't staged still
  shows up. Bound it; if per-build walking a large tree ever hurts, cache and
  invalidate on the agent's own writes — don't pre-optimize.
- **Self-edits are free.** Edit the root `AGENTS.md` → `localAgents` recomputes
  and respects it next turn; create a nested one → the inventory shows it next
  turn. No special-casing.

**Scope is broad because it's lazy.** Unlike `localAgents` (content load, narrow
— first root only, to keep eager tokens on the primary project), the inventory
walks **all** of `editable_roots()`. The workspace can legitimately span repos,
and listing a path costs nothing, so there's no reason to confine awareness to
one root.

"All paths" resolves to "all editable roots for the walk" — not the whole
filesystem, which isn't walkable. Reading is the unconfined part: `file_read`
already permits out-of-repo reads (see `search-outside-cwd`,
`out-of-repo-read-anchors`), so the agent can pull another repo's standards by
explicit path even when that repo is not an editable root and the inventory never
listed it. The inventory is proactive awareness; explicit-path reads are the
escape hatch beyond it.

## Layering summary

- **ChatSession** owns `promptSections` (deduped by `===`) and the `tools` wire
  field (serialized from the single deduped tool list). Sections are the only
  *JS* path to system-prompt content; their output renders into the Rust
  `System` message at build time (the JS `chat.push` system-role path is gone,
  the Rust role stays).
- **Sections** are stable `prompt(ctx) -> string | null` values, author-ordered
  per mode, mode-agnostic internally.
- **`toolGuidance`** is a framework section that folds families out of
  `ctx.tools`.
- **Families** are tool-attached, consumed by `toolGuidance`, never top-level.
- **ctx** carries read-only data (env + tool set); capabilities (FS via
  `discoverAgentsMd`) are imported modules.
- Immutable context up front (cache-stable); live cwd refreshed late.

## Out of scope / not yet

- A shared `Prompted` trait/interface. Revisit only when a second real section
  type makes the iterated-collection shape pay for itself.
- A `group: "<string>"` tag form of family membership — identity references only.
- Multi-family tools (a tool belongs to at most one family).
- Auto-placing `toolGuidance` — the author positions it.
- **Auto-injecting nested `AGENTS.md` content** when the agent operates in a
  subtree (track touched paths, inject the nearest once, dedup). This is the
  reliable version of nested awareness — models don't always read a path just
  because it's listed — but it adds path-tracking + dedup plumbing. Ship the
  `nestedAgentsInventory` nudge first; add auto-injection only if the nudge
  proves unreliable.
