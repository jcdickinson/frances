//! Slash-command workflows.
//!
//! A workflow is a Lua-defined hook that completely takes over a turn.
//! Workflows are declared per-id in the layered config tree as
//! `workflows.<id>.file = "/path/to/foo.lua"` and invoked from the TUI by
//! typing `/<id> [args...]`.
//!
//! This pass is scaffolding only — [`run_workflow`] just `todo!()`s. The
//! Lua runtime, history bridging, and stream-frame surface a workflow
//! gets to use are all follow-ups.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::Result;
use crate::protocol::StreamFrame;
use crate::transport::write_message;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("split workflow args: {0}")]
    SplitArgs(#[from] shell_words::ParseError),
}

/// One row of the `workflows` config table.
///
/// Each workflow owns a chunk of the per-session DB schema via the
/// migration system: `id` is its stable [`Uuid`] entity (see
/// `crate::migrations`), and `migrations` lists the SQL files in apply
/// order. Migration paths are resolved **relative to `file`'s parent
/// directory** — co-locate `0001_init.sql` with the `.lua` and refer to
/// it as `migrations = ["0001_init.sql"]`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowConfig {
    pub id: Uuid,
    pub file: PathBuf,
    /// SQL migration files, resolved relative to [`Self::file`]'s
    /// parent. Order is the apply order; once a workflow ships, treat
    /// the prefix as immutable — the migration runner refuses to load
    /// when a recorded migration's name or content drifts.
    #[serde(default)]
    pub migrations: Vec<PathBuf>,
}

/// Splits `/<name> [args...]` into the command name and its shell-split args.
///
/// Returns `Ok(None)` for plain prose or for malformed-but-not-a-command
/// input (`/`, `/  foo`, no leading slash). Returns `Err` only when the
/// input looks like a command but the args fail to shell-parse, so the
/// caller can surface a precise error to the user.
fn parse_slash_command(
    text: &str,
) -> std::result::Result<Option<(&str, Vec<String>)>, WorkflowError> {
    let Some(body) = text.strip_prefix('/') else {
        return Ok(None);
    };
    let (name, rest) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], &body[idx..]),
        None => (body, ""),
    };
    if name.is_empty() {
        return Ok(None);
    }
    let args = shell_words::split(rest.trim())?;
    Ok(Some((name, args)))
}

/// Server-side dispatch hook called from `stream_prompt`. Returns `Ok(true)`
/// when the input was a slash command (handed off to a workflow, or surfaced
/// as an unknown-command error frame). Returns `Ok(false)` when the caller
/// should fall through to the normal LLM turn.
pub async fn dispatch_slash_command(
    workflows: &HashMap<String, WorkflowConfig>,
    stream: &mut UnixStream,
    text: &str,
) -> Result<bool> {
    let (name, args) = match parse_slash_command(text) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(false),
        Err(error) => {
            write_message(
                stream,
                &StreamFrame::Error(format!("bad workflow args: {error}")),
            )
            .await?;
            return Ok(true);
        }
    };

    let Some(cfg) = workflows.get(name) else {
        write_message(
            stream,
            &StreamFrame::Error(format!("unknown workflow: {name}")),
        )
        .await?;
        return Ok(true);
    };

    run_workflow(stream, name, &args, cfg).await?;
    Ok(true)
}

async fn run_workflow(
    _stream: &mut UnixStream,
    _name: &str,
    _args: &[String],
    _cfg: &WorkflowConfig,
) -> Result<()> {
    todo!("lua workflow execution not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Option<(String, Vec<String>)> {
        parse_slash_command(text)
            .expect("parse should not error")
            .map(|(n, a)| (n.to_string(), a))
    }

    #[test]
    fn bare_name() {
        assert_eq!(parse("/plan"), Some(("plan".into(), vec![])));
    }

    #[test]
    fn name_and_args() {
        assert_eq!(
            parse("/plan foo bar"),
            Some(("plan".into(), vec!["foo".into(), "bar".into()])),
        );
    }

    #[test]
    fn quoted_arg_collapses() {
        assert_eq!(
            parse(r#"/plan "two words""#),
            Some(("plan".into(), vec!["two words".into()])),
        );
    }

    #[test]
    fn unterminated_quote_errors() {
        assert!(parse_slash_command("/plan 'unterminated").is_err());
    }

    #[test]
    fn slash_alone_is_not_a_command() {
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn slash_with_only_whitespace_name_is_not_a_command() {
        assert_eq!(parse("/ foo"), None);
    }

    #[test]
    fn plain_text_is_not_a_command() {
        assert_eq!(parse("hello there"), None);
    }

    #[test]
    fn leading_whitespace_does_not_strip() {
        // Match the user's literal input — a leading space means it isn't a
        // slash command. Don't silently re-interpret.
        assert_eq!(parse("  /plan"), None);
    }
}
