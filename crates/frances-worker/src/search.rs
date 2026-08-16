use std::io::Read;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

use frances_core::{expand_tilde, resolve_relative};
use frances_worker_protocol::{
    ErrorCode, FileSearchEvent, FileSearchFile, FileSearchMatch, FileSearchMatchMode,
    FileSearchOptions, FileSearchQuery,
};
use globset::{Glob, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Sink, SinkMatch};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use thiserror::Error;

const RESULT_CAP: NonZeroUsize = NonZeroUsize::new(1000).unwrap();
const MATCH_TEXT_CAP: usize = 512;

type Emit = dyn Fn(FileSearchEvent) -> bool + Send + Sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    Done { truncated_at: Option<NonZeroUsize> },
    Cancelled,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("root {path:?}: {source}")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("root {0:?} is not a directory")]
    RootNotDirectory(PathBuf),
    #[error("invalid exclude {pattern:?}: {source}")]
    InvalidExclude {
        pattern: String,
        #[source]
        source: ignore::Error,
    },
    #[error("build overrides: {0}")]
    BuildOverrides(#[source] ignore::Error),
    #[error("invalid glob {pattern:?}: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("build glob set: {0}")]
    BuildGlobSet(#[source] globset::Error),
    #[error("invalid regex {pattern:?}: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: grep_regex::Error,
    },
}

impl SearchError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Root { .. } => ErrorCode::Io,
            Self::RootNotDirectory(_)
            | Self::InvalidExclude { .. }
            | Self::BuildOverrides(_)
            | Self::InvalidGlob { .. }
            | Self::BuildGlobSet(_)
            | Self::InvalidRegex { .. } => ErrorCode::InvalidRequest,
        }
    }
}

/// Stream a complete find/grep operation from the worker's filesystem.
///
/// The caller runs this synchronous function on a blocking thread. `emit`
/// applies bounded backpressure and returns `false` when the receiver closes.
pub fn find_or_grep(
    options: FileSearchOptions,
    cancelled: impl Fn() -> bool + Send + Sync,
    emit: impl Fn(FileSearchEvent) -> bool + Send + Sync + 'static,
) -> Result<SearchOutcome, SearchError> {
    let cancelled = Arc::new(cancelled);
    let emit: Arc<Emit> = Arc::new(emit);
    let (paths, search) = match options.query {
        FileSearchQuery::All => (Vec::new(), None),
        FileSearchQuery::Paths { patterns } => (patterns.into_vec(), None),
        FileSearchQuery::Search {
            regex,
            paths,
            matches,
        } => (paths, Some((regex, matches))),
    };

    let root = resolve_root(options.root.as_deref(), options.cwd.as_deref())?;
    let exclude_override = if options.exclude.is_empty() {
        None
    } else {
        let mut builder = OverrideBuilder::new(&root);
        for pattern in &options.exclude {
            builder
                .add(&format!("!{pattern}"))
                .map_err(|source| SearchError::InvalidExclude {
                    pattern: pattern.clone(),
                    source,
                })?;
        }
        Some(builder.build().map_err(SearchError::BuildOverrides)?)
    };

    let include_set = if paths.is_empty() {
        None
    } else {
        let mut builder = GlobSetBuilder::new();
        for pattern in paths {
            let glob = Glob::new(&pattern)
                .map_err(|source| SearchError::InvalidGlob { pattern, source })?;
            builder.add(glob);
        }
        Some(builder.build().map_err(SearchError::BuildGlobSet)?)
    };

    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(!options.hidden)
        .git_ignore(options.ignore)
        .git_global(options.ignore)
        .git_exclude(options.ignore)
        .parents(options.ignore)
        .ignore(options.ignore)
        .require_git(false);
    if let Some(overrides) = exclude_override {
        builder.overrides(overrides);
    }
    if let Some(depth) = options.depth {
        builder.max_depth(Some(depth));
    }

    let (matcher, match_mode) = match search {
        Some((pattern, mode)) => {
            let matcher = RegexMatcher::new(&pattern)
                .map_err(|source| SearchError::InvalidRegex { pattern, source })?;
            (Some(matcher), Some(mode))
        }
        None => (None, None),
    };
    let reserved = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicBool::new(false));
    let include_set = Arc::new(include_set);
    let root_for_visitors = root.clone();

    builder.build_parallel().run(|| {
        let cancelled = cancelled.clone();
        let emit = emit.clone();
        let reserved = reserved.clone();
        let truncated = truncated.clone();
        let matcher = matcher.clone();
        let include_set = include_set.clone();
        let root = root_for_visitors.clone();
        let mut searcher = grep_searcher::SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |entry| {
            if cancelled() {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(?error, "skipping unreadable search entry");
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            if let Some(patterns) = include_set.as_ref().as_ref() {
                let relative = path.strip_prefix(&root).unwrap_or(path);
                if !patterns.is_match(relative) {
                    return WalkState::Continue;
                }
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::debug!(?path, ?error, "skipping search entry without metadata");
                    return WalkState::Continue;
                }
            };
            let file = FileSearchFile {
                path: path.strip_prefix(&root).unwrap_or(path).to_path_buf(),
                size: metadata.len(),
                mtime_ns: modified_unix_ns(path, &metadata),
            };

            if let (Some(matcher), Some(mode)) = (matcher.as_ref(), match_mode) {
                if is_binary_quick(path) {
                    return WalkState::Continue;
                }
                let mut sink = MatchSink::new(
                    matcher,
                    mode,
                    file,
                    emit.as_ref(),
                    reserved.as_ref(),
                    truncated.as_ref(),
                );
                let searched = searcher.search_path(matcher, path, &mut sink);
                if sink.receiver_closed || cancelled() {
                    return WalkState::Quit;
                }
                if sink.hit_limit {
                    return WalkState::Quit;
                }
                if let Err(error) = searched {
                    tracing::debug!(?path, ?error, "skipping unreadable search content");
                }
                return WalkState::Continue;
            }

            let slot = reserved.fetch_add(1, Ordering::Relaxed);
            if slot >= RESULT_CAP.get() {
                truncated.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let binary = is_binary_quick(path);
            if !emit(FileSearchEvent::Listed { file, binary }) {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });

    if cancelled() {
        return Ok(SearchOutcome::Cancelled);
    }
    Ok(SearchOutcome::Done {
        truncated_at: truncated.load(Ordering::Relaxed).then_some(RESULT_CAP),
    })
}

fn resolve_root(root: Option<&Path>, cwd: Option<&Path>) -> Result<PathBuf, SearchError> {
    match root {
        Some(root) if !root.as_os_str().is_empty() => {
            let expanded = expand_tilde(root);
            let resolved = resolve_relative(&expanded, cwd);
            let canonical = resolved
                .canonicalize()
                .map_err(|source| SearchError::Root {
                    path: resolved,
                    source,
                })?;
            if !canonical.is_dir() {
                return Err(SearchError::RootNotDirectory(canonical));
            }
            Ok(canonical)
        }
        _ => Ok(cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))),
    }
}

struct MatchSink<'a> {
    matcher: &'a RegexMatcher,
    mode: FileSearchMatchMode,
    file: FileSearchFile,
    emit: &'a Emit,
    reserved: &'a AtomicUsize,
    truncated: &'a AtomicBool,
    accepted: bool,
    receiver_closed: bool,
    hit_limit: bool,
}

impl<'a> MatchSink<'a> {
    fn new(
        matcher: &'a RegexMatcher,
        mode: FileSearchMatchMode,
        file: FileSearchFile,
        emit: &'a Emit,
        reserved: &'a AtomicUsize,
        truncated: &'a AtomicBool,
    ) -> Self {
        Self {
            matcher,
            mode,
            file,
            emit,
            reserved,
            truncated,
            accepted: false,
            receiver_closed: false,
            hit_limit: false,
        }
    }
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if !self.accepted {
            let slot = self.reserved.fetch_add(1, Ordering::Relaxed);
            if slot >= RESULT_CAP.get() {
                self.truncated.store(true, Ordering::Relaxed);
                self.hit_limit = true;
                return Ok(false);
            }
            self.accepted = true;
        }

        let event = match self.mode {
            FileSearchMatchMode::Count => FileSearchEvent::Counted {
                file: self.file.clone(),
            },
            FileSearchMatchMode::Content => {
                let (text, line_bytes) = match_excerpt(self.matcher, matched.bytes());
                FileSearchEvent::Matched {
                    file: self.file.clone(),
                    matched: FileSearchMatch {
                        line: matched
                            .line_number()
                            .and_then(NonZeroU64::new)
                            .expect("searcher configured to report nonzero line numbers"),
                        text,
                        line_bytes,
                    },
                }
            }
        };
        if !(self.emit)(event) {
            self.receiver_closed = true;
            return Ok(false);
        }
        Ok(true)
    }
}

fn match_excerpt(matcher: &RegexMatcher, bytes: &[u8]) -> (String, Option<NonZeroUsize>) {
    let bytes = trim_line_terminator(bytes);
    if bytes.len() <= MATCH_TEXT_CAP {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        let expanded_past_cap = text.len() > MATCH_TEXT_CAP;
        truncate_utf8(&mut text, MATCH_TEXT_CAP);
        return (
            text,
            expanded_past_cap.then(|| {
                NonZeroUsize::new(bytes.len()).expect("truncated line has nonzero length")
            }),
        );
    }

    const ELLIPSIS: &str = "…";
    let source_cap = MATCH_TEXT_CAP - (ELLIPSIS.len() * 2);
    let match_start = matcher
        .find(bytes)
        .ok()
        .flatten()
        .map_or(0, |matched| matched.start());
    let start = match_start
        .saturating_sub(source_cap / 3)
        .min(bytes.len() - source_cap);
    let end = start + source_cap;
    let mut text = String::with_capacity(MATCH_TEXT_CAP);
    if start > 0 {
        text.push_str(ELLIPSIS);
    }
    text.push_str(&String::from_utf8_lossy(&bytes[start..end]));
    if end < bytes.len() {
        text.push_str(ELLIPSIS);
    }
    truncate_utf8(&mut text, MATCH_TEXT_CAP);
    (
        text,
        Some(NonZeroUsize::new(bytes.len()).expect("truncated line has nonzero length")),
    )
}

fn trim_line_terminator(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn truncate_utf8(text: &mut String, byte_cap: usize) {
    if text.len() <= byte_cap {
        return;
    }
    let mut end = byte_cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn is_binary_quick(path: &Path) -> bool {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::debug!(?path, ?error, "could not inspect file for binary content");
            return false;
        }
    };
    let mut buffer = [0; 8192];
    let read = match file.read(&mut buffer) {
        Ok(read) => read,
        Err(error) => {
            tracing::debug!(?path, ?error, "could not inspect file for binary content");
            return false;
        }
    };
    buffer[..read].contains(&0)
}

fn modified_unix_ns(path: &Path, metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            tracing::debug!(?path, ?error, "file has no usable modification time");
            return None;
        }
    };
    let elapsed = match modified.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed,
        Err(error) => {
            tracing::debug!(
                ?path,
                ?error,
                "file modification time predates the Unix epoch"
            );
            return None;
        }
    };
    match i64::try_from(elapsed.as_nanos()) {
        Ok(nanoseconds) => Some(nanoseconds),
        Err(error) => {
            tracing::debug!(
                ?path,
                ?error,
                "file modification time does not fit i64 nanoseconds"
            );
            None
        }
    }
}
