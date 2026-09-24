use std::hash::Hasher;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use frances_edit::{LoopKey, LoopKind};
use frances_worker_protocol::{
    FileSearchMatchMode, FileSearchOptions, FileSearchPatterns, FileSearchQuery,
};
use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;

use crate::deps::{EditorSession, HarnessDeps};
use crate::io::{FileSearchResult, FileSearchResultKind, FileSearchResults, HarnessFs};

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
    fn into_options(self, cwd: Option<PathBuf>) -> Result<FileSearchOptions, SearchError> {
        let query = match (self.paths, self.search) {
            (None, None) => FileSearchQuery::All,
            (Some(paths), None) => {
                let Some(patterns) = FileSearchPatterns::new(paths) else {
                    return Err(SearchError::EmptyPaths);
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

pub async fn search_inner<D: HarnessDeps>(
    deps: &D,
    session: &EditorSession<D>,
    raw: serde_json::Value,
) -> Result<String, SearchError> {
    let args: FileSearchArgs = serde_json::from_value(raw)?;
    let key = LoopKey::Search {
        args_hash: hash_search_args(&args),
    };
    if session.lock().await.is_loop(&key) {
        return Err(SearchError::Loop);
    }

    let options = args.into_options(deps.current_cwd())?;
    let result = deps.fs().find_or_grep(options).await?;
    let payload = Payload::from(result);
    let json = serde_json::to_string(&payload)?;
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

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid search arguments or result: {0}")]
    Json(#[from] serde_json::Error),
    #[error("file search failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("paths must not be empty")]
    EmptyPaths,
    #[error("loop guard: this exact search was just performed; change the query or move on")]
    Loop,
}
