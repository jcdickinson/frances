//! Rust-backed instruction-discovery primitives for `frances:v1/agents`.
//!
//! Exposes three async JS functions placed on the install stash:
//!
//! - `_discoverGlobalAgents` — walks XDG dirs + home for `AGENTS.md` / `CLAUDE.md`
//! - `_discoverLocalAgents` — walks the first editable root for project-level files
//! - `_discoverNestedAgents` — walks all editable roots for nested `AGENTS.md`
//!
//! Each returns `null` (JS) when no files are found, or an array of
//! `{ path, content }` objects (global/local) or path strings (nested).
//!
//! Dedup is per-scope:
//! 1. Canonicalize every candidate path; drop duplicate canonical paths.
//! 2. Content-hash dedup among survivors, keeping the first (lowest-priority).

use std::collections::HashSet;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

use rquickjs::function::Opt;
use rquickjs::promise::Promised;
use rquickjs::{Array, Ctx, Function, IntoJs, Object, Result as JsResult, Value};
use twox_hash::XxHash64;

use crate::deps::WorkflowDeps;
use crate::io::WorkflowFs;
use crate::WorkflowError;


// ---------------------------------------------------------------------------
// Public entry point — called from `install_stash`
// ---------------------------------------------------------------------------

/// Build the three discovery functions and return them for placement on
/// the install stash.
pub(crate) fn build_agents_functions<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> Result<(Function<'js>, Function<'js>, Function<'js>), WorkflowError> {
    let global_fn = build_discover_global(ctx, deps.clone())?;
    let local_fn = build_discover_local(ctx, deps.clone())?;
    let nested_fn = build_discover_nested(ctx, deps)?;
    Ok((global_fn, local_fn, nested_fn))
}

// ---------------------------------------------------------------------------
// Wrapper types for IntoJs
// ---------------------------------------------------------------------------

/// Wrapper for content results: `Vec<(PathBuf, String)>` → JS `Array<{ path, content }>` or `null`.
struct ContentResults(Vec<(PathBuf, String)>);

impl<'js> IntoJs<'js> for ContentResults {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        if self.0.is_empty() {
            return Ok(Value::new_null(ctx.clone()));
        }
        let arr = Array::new(ctx.clone())?;
        for (i, (path, content)) in self.0.into_iter().enumerate() {
            let obj = Object::new(ctx.clone())?;
            obj.set("path", path.to_string_lossy().as_ref())?;
            obj.set("content", content)?;
            arr.set(i, obj)?;
        }
        Ok(arr.into_value())
    }
}

/// Wrapper for path results: `Vec<String>` → JS `Array<string>` or `null`.
struct PathResults(Vec<String>);

impl<'js> IntoJs<'js> for PathResults {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        if self.0.is_empty() {
            return Ok(Value::new_null(ctx.clone()));
        }
        let arr = Array::new(ctx.clone())?;
        for (i, path) in self.0.into_iter().enumerate() {
            arr.set(i, path)?;
        }
        Ok(arr.into_value())
    }
}

// ---------------------------------------------------------------------------
// discoverGlobalAgents
// ---------------------------------------------------------------------------

fn build_discover_global<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> Result<Function<'js>, WorkflowError> {
    let ctx = ctx.clone();
    let f = Function::new(ctx, move |ctx: Ctx<'js>, _arg: Opt<Value<'js>>| -> JsResult<Value<'js>> {
        let deps = deps.clone();
        let promised = Promised::from(async move {
            let candidates = global_candidates();
            ContentResults(discover_with_content(&deps, candidates).await)
        });
        promised.into_js(&ctx)
    })?;
    Ok(f)
}

/// Build the ordered list of candidate paths for global agent instructions.
///
/// Order: lowest-priority → highest-priority.
///
/// 1. `~/.claude/CLAUDE.md` (lowest)
/// 2. `XDG_CONFIG_DIRS` (system; reversed so lowest-priority first):
///    `<dir>/AGENTS.md`, then `<dir>/frances/AGENTS.md`.
/// 3. `XDG_CONFIG_HOME` (user): `AGENTS.md`, then `frances/AGENTS.md`.
/// 4. `$HOME/AGENTS.md` (highest).
fn global_candidates() -> Vec<PathBuf> {
    let xdg = xdg::BaseDirectories::new();
    let home = dirs_home();
    let mut candidates = Vec::new();

    // 1. ~/.claude/CLAUDE.md (lowest)
    candidates.push(home.join(".claude").join("CLAUDE.md"));

    // 2. System XDG config dirs. The xdg crate's get_config_dirs()
    //    returns user home first, then system dirs. We split them and
    //    reverse system dirs so lowest-priority comes first.
    let all_config_dirs = xdg.get_config_dirs();
    let config_home = xdg.get_config_home();
    let system_dirs: Vec<PathBuf> = all_config_dirs
        .into_iter()
        .filter(|d| config_home.as_ref() != Some(d))
        .collect();
    for dir in system_dirs.into_iter().rev() {
        // Generic before frances-specific (less specific → more specific).
        candidates.push(dir.join("AGENTS.md"));
        candidates.push(dir.join("frances").join("AGENTS.md"));
    }

    // 3. XDG_CONFIG_HOME (user dir; outranks all system dirs).
    if let Some(ch) = config_home {
        candidates.push(ch.join("AGENTS.md"));
        candidates.push(ch.join("frances").join("AGENTS.md"));
    }

    // 4. $HOME/AGENTS.md (highest).
    candidates.push(home.join("AGENTS.md"));

    candidates
}

// ---------------------------------------------------------------------------
// discoverLocalAgents
// ---------------------------------------------------------------------------

fn build_discover_local<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> Result<Function<'js>, WorkflowError> {
    let ctx = ctx.clone();
    let f = Function::new(ctx, move |ctx: Ctx<'js>, _arg: Opt<Value<'js>>| -> JsResult<Value<'js>> {
        let deps = deps.clone();
        let promised = Promised::from(async move {
            let Some(root) = deps.editable_roots().first().cloned() else {
                return ContentResults(Vec::new());
            };
            let candidates = local_candidates(&root);
            ContentResults(discover_with_content(&deps, candidates).await)
        });
        promised.into_js(&ctx)
    })?;
    Ok(f)
}

/// Build the ordered list of candidate paths for local agent instructions,
/// rooted at the first editable root.
///
/// Order: lowest-priority → highest-priority.
///
/// 1. `root/CLAUDE.md`, then `root/CLAUDE.local.md`
/// 2. `root/AGENTS.md`, then `root/AGENTS.local.md`
/// 3. `root/.agents/frances/AGENTS.md`, then `root/.agents/frances/AGENTS.local.md`
fn local_candidates(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("CLAUDE.md"),
        root.join("CLAUDE.local.md"),
        root.join("AGENTS.md"),
        root.join("AGENTS.local.md"),
        root.join(".agents").join("frances").join("AGENTS.md"),
        root.join(".agents")
            .join("frances")
            .join("AGENTS.local.md"),
    ]
}

// ---------------------------------------------------------------------------
// discoverNestedAgents
// ---------------------------------------------------------------------------

fn build_discover_nested<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> Result<Function<'js>, WorkflowError> {
    let ctx = ctx.clone();
    let f = Function::new(ctx, move |ctx: Ctx<'js>, _arg: Opt<Value<'js>>| -> JsResult<Value<'js>> {
        let deps = deps.clone();
        let promised = Promised::from(async move {
            PathResults(discover_nested(&deps).await)
        });
        promised.into_js(&ctx)
    })?;
    Ok(f)
}

/// Walk all editable roots for nested `AGENTS.md` files (depth > 0).
/// Returns deduplicated canonical paths as strings.
async fn discover_nested<D: WorkflowDeps>(deps: &D) -> Vec<String> {
    let roots = deps.editable_roots();
    if roots.is_empty() {
        return Vec::new();
    }

    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut results: Vec<String> = Vec::new();

    // Build the set of root-level canonical paths to exclude.
    let mut root_level: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        for name in &[
            "AGENTS.md",
            "AGENTS.local.md",
            "CLAUDE.md",
            "CLAUDE.local.md",
        ] {
            let p = root.join(name);
            if let Ok(canonical) = deps.fs().canonicalize(&p).await {
                root_level.insert(canonical);
            }
        }
        let frances = root.join(".agents").join("frances");
        for name in &["AGENTS.md", "AGENTS.local.md"] {
            let p = frances.join(name);
            if let Ok(canonical) = deps.fs().canonicalize(&p).await {
                root_level.insert(canonical);
            }
        }
    }

    for root in roots {
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false) // include dotfiles like .agents/AGENTS.md
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for entry in walker.flatten() {
            // Skip if not a file or if at root depth.
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if entry.depth() < 1 {
                continue;
            }

            let path = entry.path();

            // Only match AGENTS.md files (case-sensitive).
            if path.file_name().is_some_and(|n| n == "AGENTS.md") {
                let canonical = deps
                    .fs()
                    .canonicalize(path)
                    .await
                    .unwrap_or_else(|_| path.to_path_buf());

                // Exclude root-level files already covered by localAgents.
                if root_level.contains(&canonical) {
                    continue;
                }

                if seen_canonical.insert(canonical) {
                    results.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Shared discovery + dedup logic
// ---------------------------------------------------------------------------

/// For each candidate path (in order):
/// 1. Try to canonicalize; if the file doesn't exist, skip.
/// 2. Dedup canonical paths (keep first occurrence = lowest priority).
/// 3. Read surviving files.
/// 4. Content-hash dedup (keep first occurrence).
///
/// Returns `Vec<(PathBuf, String)>` — (display path, content) pairs.
async fn discover_with_content<D: WorkflowDeps>(
    deps: &D,
    candidates: Vec<PathBuf>,
) -> Vec<(PathBuf, String)> {
    // Phase 1: canonicalize + path dedup.
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut unique_paths: Vec<PathBuf> = Vec::new();

    for candidate in &candidates {
        let canonical = match deps.fs().canonicalize(candidate).await {
            Ok(c) => c,
            Err(_) => continue, // file doesn't exist; skip
        };
        if seen_canonical.insert(canonical) {
            unique_paths.push(candidate.clone());
        }
    }

    // Phase 2: read + content-hash dedup.
    let mut seen_hashes: HashSet<u64> = HashSet::new();
    let mut results: Vec<(PathBuf, String)> = Vec::new();

    for path in &unique_paths {
        let content = match deps.fs().read_to_string(path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let hash = content_hash(&content);
        if seen_hashes.insert(hash) {
            results.push((path.clone(), content));
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn content_hash(content: &str) -> u64 {
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(content.as_bytes());
    hasher.finish()
}

/// Best-effort home directory. Tries `$HOME`, falls back to `/tmp`.
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
