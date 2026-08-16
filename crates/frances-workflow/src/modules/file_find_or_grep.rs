//! `frances:v1/tools/file_find_or_grep` — combined name-pattern lookup,
//! content search, and directory listing primitive.
//!
//! `new FileSearch(editor)` exposes one async `search(args)` method. The
//! complete filesystem operation is delegated through [`WorkflowFs`], so
//! production performs the walk and grep in the worker. Results cross the
//! worker protocol as typed entries and become one JSON string at the JS
//! boundary.

use std::hash::Hasher;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use frances_core::JsonRepair;
use frances_edit::{LoopKey, LoopKind};
use frances_worker_protocol::{
    FileSearchMatchMode, FileSearchOptions, FileSearchPatterns, FileSearchQuery,
};
use rquickjs::class::{JsClass, Readable, Trace, Tracer};
use rquickjs::function::{Constructor, This};
use rquickjs::promise::Promised;
use rquickjs::{Class, Ctx, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};
use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;

use super::file::EditorJs;
use super::throw_js as throw;
use crate::deps::{EditorSession, WorkflowDeps};
use crate::io::{FileSearchResult, FileSearchResultKind, FileSearchResults, WorkflowFs};

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct FileSearchArgs {
    root: Option<String>,
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

impl Default for FileSearchArgs {
    fn default() -> Self {
        Self {
            root: None,
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

impl FileSearchArgs {
    fn into_options(self, cwd: Option<PathBuf>) -> Result<FileSearchOptions, &'static str> {
        let query = match (self.paths, self.search) {
            (None, None) => FileSearchQuery::All,
            (Some(paths), None) => {
                let Some(patterns) = FileSearchPatterns::new(paths) else {
                    return Err(
                        "provide at least one of \"paths\" or \"search\", or call with no arguments",
                    );
                };
                FileSearchQuery::Paths { patterns }
            }
            (paths, Some(regex)) => FileSearchQuery::Search {
                regex,
                paths: paths.unwrap_or_default(),
                matches: if self.paths_only {
                    FileSearchMatchMode::Count
                } else {
                    FileSearchMatchMode::Content
                },
            },
        };
        Ok(FileSearchOptions {
            cwd,
            root: self.root.filter(|root| !root.is_empty()).map(PathBuf::from),
            query,
            exclude: self.exclude.unwrap_or_default(),
            ignore: self.ignore,
            hidden: self.hidden,
            depth: self.depth,
        })
    }
}

pub(crate) fn build_file_search_ctor<'js, D: WorkflowDeps>(
    ctx: &Ctx<'js>,
    deps: D,
) -> JsResult<Constructor<'js>> {
    Constructor::new_class::<FileSearchJs<D>, _, _>(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              editor: Class<'js, EditorJs<D>>|
              -> JsResult<Class<'js, FileSearchJs<D>>> {
            let session = editor.borrow().session.clone();
            Class::instance(
                ctx.clone(),
                FileSearchJs {
                    deps: deps.clone(),
                    session,
                },
            )
        },
    )
}

pub struct FileSearchJs<D: WorkflowDeps> {
    deps: D,
    session: EditorSession<D>,
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
        let prototype = Object::new(ctx.clone())?;
        prototype.set(
            "search",
            Function::new(
                ctx.clone(),
                |this: This<Class<'js, FileSearchJs<D>>>, value: Value<'js>| {
                    let raw = super::rquickjs_to_json(&value);
                    let search = this.0.borrow();
                    let deps = search.deps.clone();
                    let session = search.session.clone();
                    drop(search);
                    Ok::<_, rquickjs::Error>(Promised::from(async move {
                        let result = match raw {
                            Ok(value) => search_inner(&deps, &session, value).await,
                            Err(message) => Err(message),
                        };
                        SearchStringResult(result)
                    }))
                },
            )?,
        )?;
        Ok(Some(prototype))
    }

    fn constructor(_ctx: &Ctx<'js>) -> JsResult<Option<Constructor<'js>>> {
        Ok(None)
    }
}

async fn search_inner<D: WorkflowDeps>(
    deps: &D,
    session: &EditorSession<D>,
    raw: serde_json::Value,
) -> Result<String, String> {
    let args = JsonRepair::<FileSearchArgs>::from_value(raw)
        .map_err(|error| format!("parse args: {error}"))?
        .into_inner();
    let key = LoopKey::Search {
        args_hash: hash_search_args(&args),
    };
    if session.lock().await.is_loop(&key) {
        return Err(loop_error_search().to_owned());
    }

    let options = args
        .into_options(deps.current_cwd())
        .map_err(str::to_owned)?;
    let result = deps
        .fs()
        .find_or_grep(options)
        .await
        .map_err(|error| error.to_string())?;
    let payload = Payload::from(result);
    let json = serde_json::to_string(&payload).map_err(|error| format!("serialize: {error}"))?;
    session.lock().await.record_loop(key);
    Ok(json)
}

fn hash_search_args(args: &FileSearchArgs) -> u64 {
    let mut hasher = XxHash3_64::new();
    hasher.write(&[LoopKind::Search as u8]);
    if let Some(root) = &args.root {
        hasher.write(root.as_bytes());
    }
    hasher.write(&[0xFE]);
    hash_str_list_sorted(&mut hasher, args.paths.as_deref());
    hasher.write(&[0xFE]);
    if let Some(search) = &args.search {
        hasher.write(search.as_bytes());
    }
    hasher.write(&[0xFE]);
    hash_str_list_sorted(&mut hasher, args.exclude.as_deref());
    hasher.write(&[0xFE]);
    hasher.write(&[u8::from(args.ignore), u8::from(args.hidden)]);
    if let Some(depth) = args.depth {
        hasher.write(&depth.to_le_bytes());
    }
    hasher.write(&[0xFE]);
    hasher.write(&[u8::from(args.paths_only)]);
    hasher.finish()
}

fn hash_str_list_sorted(hasher: &mut XxHash3_64, items: Option<&[String]>) {
    let Some(items) = items else {
        return;
    };
    let mut sorted: Vec<&String> = items.iter().collect();
    sorted.sort();
    for item in sorted {
        hasher.write(item.as_bytes());
        hasher.write(&[0]);
    }
}

fn loop_error_search() -> &'static str {
    "loop guard: this exact search was just performed and the workspace has \
     not changed since. you already have the result. do something different \
     — change the query, the paths, or the tool, or move on."
}

#[derive(Serialize)]
struct Entry {
    path: String,
    size: u64,
    mtime: String,
    binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_count: Option<NonZeroU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_match: Option<FirstMatch>,
}

#[derive(Serialize)]
struct FirstMatch {
    line: NonZeroU64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_bytes: Option<NonZeroUsize>,
}

impl From<frances_worker_protocol::FileSearchMatch> for FirstMatch {
    fn from(first_match: frances_worker_protocol::FileSearchMatch) -> Self {
        Self {
            line: first_match.line,
            text: first_match.text,
            line_bytes: first_match.line_bytes,
        }
    }
}

impl From<FileSearchResult> for Entry {
    fn from(result: FileSearchResult) -> Self {
        let (binary, match_count, first_match) = match result.kind {
            FileSearchResultKind::Listed { binary } => (binary, None, None),
            FileSearchResultKind::Counted { match_count } => (false, Some(match_count), None),
            FileSearchResultKind::Matched { match_count, first } => {
                (false, Some(match_count), Some(FirstMatch::from(first)))
            }
        };
        Self {
            path: result.file.path.to_string_lossy().into_owned(),
            size: result.file.size,
            mtime: format_mtime(result.file.mtime_ns),
            binary,
            match_count,
            first_match,
        }
    }
}

#[derive(Serialize)]
struct Payload {
    entries: Vec<Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated_at: Option<NonZeroUsize>,
}

impl From<FileSearchResults> for Payload {
    fn from(results: FileSearchResults) -> Self {
        Self {
            entries: results.entries.into_iter().map(Entry::from).collect(),
            truncated_at: results.truncated_at,
        }
    }
}

fn format_mtime(nanoseconds: Option<i64>) -> String {
    let Some(nanoseconds) = nanoseconds else {
        return String::new();
    };
    DateTime::<Utc>::from_timestamp_nanos(nanoseconds).to_rfc3339_opts(SecondsFormat::Secs, true)
}

struct SearchStringResult(Result<String, String>);

impl<'js> IntoJs<'js> for SearchStringResult {
    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        match self.0 {
            Ok(string) => string.into_js(ctx),
            Err(message) => Err(throw(ctx, &message)),
        }
    }
}
