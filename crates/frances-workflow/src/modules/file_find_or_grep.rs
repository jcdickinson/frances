//! `frances:v1/tools/file_find_or_grep` — combined name-pattern lookup,
//! content search, and directory listing primitive.
//!
//! `new FileSearch()` exposes a single async method, `search(args)`, that
//! drives `ignore::WalkParallel` (the same multi-threaded walker ripgrep
//! uses) and — when `args.search` is set — runs `grep-searcher` per-file
//! through a per-thread `Searcher`/`RegexMatcher` clone. The result is a
//! JSON string the JS wrapper parses; building a JS value tree directly
//! would be slower than `JSON.parse` of a single string for this much
//! data.
//!
//! Argument parsing goes through `frances_core::JsonRepair` so the
//! qwen3-coder family's double-encoded array args (`paths: "[\"a\"]"`
//! instead of `paths: ["a"]`) parse the same as the strict shape — at
//! zero cost on the happy path.
//!
//! Paths are resolved against the runtime's most-recently-attached
//! client cwd (`WorkflowDeps::current_cwd`), matching the file editor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use frances_core::JsonRepair;
use frances_edit::LoopKey;
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Sink, SinkMatch};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{
    Class, Ctx, Exception, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value,
};
use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;

use crate::deps::{EditorFactory, WorkflowDeps};

/// Hard cap on result entries. Workers atomically reserve a slot before
/// pushing; the (cap+1)-th reservation flips a sticky `truncated` flag
/// and the visitor returns `WalkState::Quit`. Per-thread already-found
/// entries may still trickle in before the quit propagates, so the
/// final list is trimmed to exactly `RESULT_CAP`.
const RESULT_CAP: usize = 1000;

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct FileSearchArgs {
    paths: Option<Vec<String>>,
    search: Option<String>,
    exclude: Option<Vec<String>>,
    #[serde(default = "default_true")]
    ignore: bool,
    #[serde(default)]
    hidden: bool,
    depth: Option<usize>,
    #[serde(default)]
    paths_only: bool,
}

// Hand-written so `Default::default()` matches the serde defaults
// (`ignore: true`). `#[derive(Default)]` would give `ignore: false`
// because `#[serde(default = ...)]` only fires during deserialization,
// not for `Default::default()` — easy to footgun.
impl Default for FileSearchArgs {
    fn default() -> Self {
        Self {
            paths: None,
            search: None,
            exclude: None,
            ignore: true,
            hidden: false,
            depth: None,
            paths_only: false,
        }
    }
}

pub(crate) fn build_file_search_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<FileSearchJs<D>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> JsResult<Class<'js, FileSearchJs<D>>> {
            Class::instance(ctx.clone(), FileSearchJs { deps: deps.clone() })
        },
    )
}

pub struct FileSearchJs<D: WorkflowDeps> {
    deps: D,
}

impl<'js, D: WorkflowDeps> Trace<'js> for FileSearchJs<D> {
    fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js, D: WorkflowDeps> JsLifetime<'js> for FileSearchJs<D> {
    type Changed<'to> = FileSearchJs<D>;
}

impl<'js, D: WorkflowDeps> JsClass<'js> for FileSearchJs<D> {
    const NAME: &'static str = "FileSearch";
    type Mutable = Readable;

    fn prototype(ctx: &Ctx<'js>) -> JsResult<Option<Object<'js>>> {
        let proto = Object::new(ctx.clone())?;

        proto.set(
            "search",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, FileSearchJs<D>>>, value: Value<'js>| {
                    let raw = super::rquickjs_to_json(&value);
                    let deps = this.0.borrow().deps.clone();
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(v) => search_inner(&deps, v).await,
                            Err(msg) => Err(msg),
                        };
                        SearchStringResult(result)
                    }))
                },
            )?,
        )?;

        Ok(Some(proto))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

async fn search_inner<D: WorkflowDeps>(deps: &D, raw: serde_json::Value) -> Result<String, String> {
    let args = JsonRepair::<FileSearchArgs>::from_value(raw)
        .map_err(|e| format!("parse args: {e}"))?
        .into_inner();
    let key = LoopKey::Search {
        args_hash: hash_search_args(&args),
    };

    let session = deps.editor_factory().session();
    if session.lock().await.is_loop(&key) {
        return Err(loop_error_search().to_string());
    }

    let cwd = deps.current_cwd();
    let result = tokio::task::spawn_blocking(move || do_search(args, cwd.as_deref()))
        .await
        .map_err(|e| format!("join: {e}"))??;
    session.lock().await.record_loop(key);
    Ok(result)
}

fn hash_search_args(args: &FileSearchArgs) -> u64 {
    // Hand-pack a canonical byte sequence rather than serde_json so
    // the layout is fixed regardless of field-order drift in
    // `FileSearchArgs`. Order matches the struct fields.
    let mut buf = Vec::with_capacity(64);
    // Discriminator — keeps file_find_or_grep keys from colliding with
    // anything else that might one day land in this hasher.
    buf.push(2u8);
    push_str_list_sorted(&mut buf, args.paths.as_deref());
    buf.push(0xFE);
    if let Some(s) = &args.search {
        buf.extend_from_slice(s.as_bytes());
    }
    buf.push(0xFE);
    push_str_list_sorted(&mut buf, args.exclude.as_deref());
    buf.push(0xFE);
    buf.push(u8::from(args.ignore));
    buf.push(u8::from(args.hidden));
    if let Some(d) = args.depth {
        buf.extend_from_slice(&d.to_le_bytes());
    }
    buf.push(0xFE);
    buf.push(u8::from(args.paths_only));
    XxHash3_64::oneshot(&buf)
}

fn push_str_list_sorted(buf: &mut Vec<u8>, items: Option<&[String]>) {
    let Some(items) = items else {
        return;
    };
    let mut sorted: Vec<&String> = items.iter().collect();
    sorted.sort();
    for item in sorted {
        buf.extend_from_slice(item.as_bytes());
        buf.push(0);
    }
}

fn loop_error_search() -> &'static str {
    "loop guard: this exact search was just performed and the workspace has \
     not changed since. you already have the result. do something different \
     — change the query, the paths, or the tool, or move on."
}

#[derive(Serialize, Debug)]
struct Entry {
    path: String,
    size: u64,
    mtime: String,
    binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_match: Option<FirstMatch>,
}

#[derive(Serialize, Debug)]
struct FirstMatch {
    line: u64,
    text: String,
}

#[derive(Serialize, Debug)]
struct Truncated {
    count: String,
    message: String,
}

#[derive(Serialize, Debug)]
struct Payload {
    entries: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<Truncated>,
}

fn do_search(args: FileSearchArgs, cwd: Option<&Path>) -> Result<String, String> {
    let (paths, search) = match (args.paths, args.search) {
        // No-args → recursive listing of pwd. Empty `paths` ↦ no
        // include-set, i.e. accept every gitignore-permitted file.
        (None, None) => (Vec::new(), None),
        // Empty paths with no search → loud error, not silent wildcard.
        (Some(p), None) if p.is_empty() => {
            return Err(
                "provide at least one of \"paths\" or \"search\", or call with no arguments"
                    .to_string(),
            );
        }
        (p, s) => (p.unwrap_or_default(), s),
    };

    let root = cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    // Excludes go through `OverrideBuilder` as negations — overrides
    // intentionally win over `.gitignore`, which is what you want for
    // an explicit "don't show me this" filter.
    //
    // Includes (`paths`) use a separate `GlobSet` that we test per
    // entry, NOT an Override whitelist. Overrides with positive
    // patterns bypass `.gitignore` (rg's `-g` semantics): a whitelist
    // of `**/*` would silently include every gitignored file. Keeping
    // includes out of the override pipeline preserves gitignore
    // precedence — `paths: ["**/*.rs"]` still excludes vendored Rust
    // under `target/`.
    let exclude_override = match args.exclude.as_ref() {
        Some(excludes) if !excludes.is_empty() => {
            let mut b = OverrideBuilder::new(&root);
            for p in excludes {
                b.add(&format!("!{p}"))
                    .map_err(|e| format!("invalid exclude {p:?}: {e}"))?;
            }
            Some(b.build().map_err(|e| format!("build overrides: {e}"))?)
        }
        _ => None,
    };

    let include_set: Option<GlobSet> = if paths.is_empty() {
        None
    } else {
        let mut b = GlobSetBuilder::new();
        for p in &paths {
            let glob = Glob::new(p).map_err(|e| format!("invalid glob {p:?}: {e}"))?;
            b.add(glob);
        }
        Some(b.build().map_err(|e| format!("build glob set: {e}"))?)
    };

    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(!args.hidden)
        .git_ignore(args.ignore)
        .git_global(args.ignore)
        .git_exclude(args.ignore)
        .parents(args.ignore)
        .ignore(args.ignore)
        // `.gitignore` is normally only consulted inside a real git tree
        // (`require_git` defaults to `true`). For an agent that may run
        // in scaffolded/non-git dirs the expected behavior is "the file
        // is still honored" — matches what ripgrep itself does.
        .require_git(false);
    if let Some(o) = exclude_override {
        builder.overrides(o);
    }
    if let Some(d) = args.depth {
        builder.max_depth(Some(d));
    }
    let walker = builder.build_parallel();

    let matcher = if let Some(ref pat) = search {
        Some(RegexMatcher::new(pat).map_err(|e| format!("invalid regex {pat:?}: {e}"))?)
    } else {
        None
    };

    let results: Arc<Mutex<Vec<Entry>>> = Arc::new(Mutex::new(Vec::new()));
    let reserved = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicUsize::new(0));
    let paths_only = args.paths_only;
    let want_search = search.is_some();
    let root_for_visitors = root.clone();
    let include_set = Arc::new(include_set);

    walker.run(|| {
        let results = results.clone();
        let reserved = reserved.clone();
        let truncated = truncated.clone();
        let matcher = matcher.clone();
        let include_set = include_set.clone();
        let root = root_for_visitors.clone();
        let mut searcher = grep_searcher::SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .build();
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            // Include-glob test against the path relative to root so
            // patterns like `src/**/*.rs` work the way the agent wrote
            // them. Falls back to the absolute path if strip fails
            // (shouldn't happen — walker yields under root).
            if let Some(set) = include_set.as_ref().as_ref() {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                if !set.is_match(rel) {
                    return WalkState::Continue;
                }
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return WalkState::Continue,
            };
            let size = meta.len();
            let mtime = meta
                .modified()
                .ok()
                .and_then(systemtime_to_iso8601)
                .unwrap_or_default();
            let binary = is_binary_quick(path);

            // rg's default: skip binary files for content search.
            if want_search && binary {
                return WalkState::Continue;
            }

            let (match_count, first_match) = if let Some(m) = matcher.as_ref() {
                let mut sink = MatchSink::default();
                if searcher.search_path(m, path, &mut sink).is_err() {
                    return WalkState::Continue;
                }
                if sink.count == 0 {
                    return WalkState::Continue;
                }
                let mc = Some(sink.count);
                let fm = if paths_only { None } else { sink.first };
                (mc, fm)
            } else {
                (None, None)
            };

            // Reserve a slot before pushing. (cap+1)-th caller flips the
            // sticky truncated flag and quits without contributing.
            let slot = reserved.fetch_add(1, Ordering::Relaxed);
            if slot >= RESULT_CAP {
                truncated.fetch_add(1, Ordering::Relaxed);
                return WalkState::Quit;
            }

            let display_path = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();

            let entry = Entry {
                path: display_path,
                size,
                mtime,
                binary,
                match_count,
                first_match,
            };
            if let Ok(mut g) = results.lock() {
                g.push(entry);
            }
            WalkState::Continue
        })
    });

    let mut entries = Arc::try_unwrap(results)
        .map_err(|_| "results arc still shared after walk".to_string())?
        .into_inner()
        .map_err(|e| format!("results mutex: {e}"))?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    // In-flight workers may have pushed past the cap before the Quit
    // propagated; trim deterministically.
    if entries.len() > RESULT_CAP {
        entries.truncate(RESULT_CAP);
    }
    let was_truncated = truncated.load(Ordering::Relaxed) > 0;
    let truncated_info = was_truncated.then(|| Truncated {
        count: format!("{RESULT_CAP}+"),
        message: format!(
            "{RESULT_CAP}+ matches, capped at {RESULT_CAP} — narrow paths or search to see all"
        ),
    });

    let payload = Payload {
        entries,
        truncated: truncated_info,
    };
    serde_json::to_string(&payload).map_err(|e| format!("serialize: {e}"))
}

#[derive(Default)]
struct MatchSink {
    count: u64,
    first: Option<FirstMatch>,
}

impl Sink for MatchSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        self.count += 1;
        if self.first.is_none() {
            let text = String::from_utf8_lossy(mat.bytes())
                .trim_end_matches(['\n', '\r'])
                .to_string();
            let line = mat.line_number().unwrap_or(0);
            self.first = Some(FirstMatch { line, text });
        }
        Ok(true)
    }
}

/// Cheap binary detection: peek the first 8 KiB and look for NUL.
/// Matches what `grep_searcher::BinaryDetection::quit` does internally
/// once it starts reading, but lets us tag files we never feed to the
/// searcher (i.e. when `search` is unset).
fn is_binary_quick(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 8192];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n].contains(&0)
}

/// ISO 8601 / RFC 3339 formatter (UTC, second precision) without a
/// chrono/time dep. Uses Howard Hinnant's civil-from-days algorithm,
/// good for the full proleptic Gregorian range we care about.
fn systemtime_to_iso8601(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    let (year, mo, day, h, m, s) = civil_from_unix(secs);
    Some(format!("{year:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"))
}

fn civil_from_unix(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let h = tod / 3600;
    let m = (tod / 60) % 60;
    let s = tod % 60;

    // Hinnant 2013: `civil_from_days`.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mo <= 2 { y + 1 } else { y };
    (year as i32, mo as u32, d as u32, h, m, s)
}

struct SearchStringResult(Result<String, String>);

impl<'js> IntoJs<'js> for SearchStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(s) => s.into_js(ctx),
            Err(msg) => Err(throw(ctx, &msg)),
        }
    }
}

fn throw<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exc) => exc.throw(),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_unix_known_dates() {
        // 1970-01-01 00:00:00 UTC
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2000-01-01 00:00:00 UTC = 946684800
        assert_eq!(civil_from_unix(946_684_800), (2000, 1, 1, 0, 0, 0));
        // 2024-02-29 12:34:56 UTC = 1709210096
        assert_eq!(civil_from_unix(1_709_210_096), (2024, 2, 29, 12, 34, 56));
    }
}
