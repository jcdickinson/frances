//! Unit tests for the workflow runtime, split by surface.
//!
//! Shared fixtures live here; each submodule pulls them in with `use super::*`.

pub(crate) use super::test_deps::StubDeps;
pub(crate) use super::test_drive::{CYCLE_TIMEOUT, drive_one_cycle, drive_to_done};
pub(crate) use super::*;
pub(crate) use crate::permission::PermissionResponse;

use std::io::Write;

mod chat;
mod complete;
mod drive;
mod frames;
mod inbox;
mod permission;
mod scope;
mod shell;
mod timer;
mod whatwg;

fn write_source(ext: &str, body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn text_of(delta: &SectionTranscript) -> String {
    match delta {
        SectionTranscript::Set { section: spec, .. } => match &spec.kind {
            SectionKind::Markdown { .. } | SectionKind::Error => {
                spec.seed.clone().unwrap_or_default()
            }
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::ShellOutput { state, cmd } => {
                format!(
                    "[shell:{state:?}] $ {cmd}\n{}",
                    spec.seed.clone().unwrap_or_default()
                )
            }
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
    }
}
