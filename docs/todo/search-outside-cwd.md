# `Search` (file_find_or_grep) cannot search outside cwd

This is the root cause of agents shelling out to `grep -r ~/.cargo` for
dependency-source lookups instead of using the `Search` tool: **the tool
genuinely cannot do it.** The fallback to shell is the only option available,
not a framing/prior problem.

## What's actually wrong

`do_search` (`crates/frances-workflow/src/modules/file_find_or_grep.rs:259`)
roots the walk at cwd, unconditionally:

```rust
let root = cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
// ...
let mut builder = ignore::WalkBuilder::new(&root);   // line ~310
```

`paths` is **not** a set of walk roots — it's a `GlobSet` applied as a
per-entry *filter*, tested against the entry path **relative to cwd**
(`file_find_or_grep.rs:368`):

```rust
let rel = path.strip_prefix(&root).unwrap_or(path);
if !set.is_match(rel) { return WalkState::Continue; }
```

So an absolute glob like `/home/jono/.cargo/registry/src/**/*.rs` matches
nothing: the walker only ever yields paths under cwd, those get stripped to
relative, and an absolute pattern can't match a relative path. There is no code
path that walks an absolute directory outside cwd.

The description lies about this. `desc/file_find_or_grep.md` says "Paths are
resolved against the client's working directory **unless absolute**" — the
"unless absolute" clause describes behaviour that does not exist.

## Tilde is also not expanded

Separately: neither `Search` nor `Read` expand `~`. `resolve_relative`
(`crates/frances-core/src/path_util.rs:8`) only branches on `is_absolute()`;
`~` is not absolute, so `~/.cargo` gets joined onto cwd → `cwd/~/.cargo` and
fails. `Glob::new("~/...")` likewise treats `~` as a literal path component.
Agents will type `~/.cargo` — decide whether to expand `~` / `~user` at the
path-resolution boundary (shared by Read and Search) or to leave it and rely on
absolute paths only.

## The fix

Make `Search` able to root the walk outside cwd. Options:

- An explicit `root` (or `roots: string[]`) arg that seeds `WalkBuilder` /
  `add()`, with `paths` remaining the relative include-filter under each root.
  Cleanest — keeps the glob-filter semantics intact and makes "search this
  external tree" a first-class, obvious shape.
- Detect absolute base segments inside `paths` and seed a walk root from each.
  Convenient (matches what the description already claims) but messier — you
  have to split the literal prefix from the glob tail per pattern.

Lean toward the explicit `root`/`roots` arg.

Watch the defaults for external trees: `ignore: true` will silently hide files
(a crate's unpacked tarball under `~/.cargo/registry/src` often ships its own
`.gitignore`), and `hidden: false` drops dot-dirs. For an out-of-cwd search the
agent will usually want `ignore: false` and often `hidden: true`. Consider
whether `ignore` should default differently when the root is outside cwd, or
just document it.

## Description changes (do alongside the code, not before)

Once `Search` can actually reach outside cwd:

- Fix the false "unless absolute" claim in `desc/file_find_or_grep.md`.
- Add an out-of-tree paragraph: this is the tool for reading dependency sources
  under `~/.cargo`, system headers, etc. — don't shell out to `grep -r`.
- Add a worked example, e.g.:

  ```
  // Read a dependency's source (external tree — disable ignore/hidden filtering)
  { root: "/home/you/.cargo/registry/src", paths: ["**/*.rs"], search: "fn poll_next", ignore: false, hidden: true }
  ```

Do NOT add this example/guidance until the code supports it — documenting a
non-existent capability sends the agent to run it, get zero results, conclude
the tool is broken, and fall back to grep with extra confusion. (Already made
and reverted that mistake once.)

## When to pick this up

Whenever we want agents to stop shelling out for dependency-source reads — this
is the blocker. Pairs with [out-of-repo-read-anchors](out-of-repo-read-anchors.md):
this todo gets the agent *finding* external source via the tool; that one keeps
*reading* it cheap.
