//! Unit tests for the workflow runtime, split by surface.
//!
//! Shared fixtures live here; each submodule pulls them in with `use super::*`.

pub(crate) use super::test_deps::StubDeps;
pub(crate) use super::test_drive::{CYCLE_TIMEOUT, drive_one_cycle, drive_to_done};
pub(crate) use super::*;
pub(crate) use crate::permission::PermissionResponse;

use std::io::Write;

mod agent_sections;
mod agents;
mod chat;
mod complete;
mod context_sections;
mod drive;
mod entities;
mod frames;
mod inbox;
mod main_workflow;
mod messages;
mod permission;
mod scope;
mod shell;
mod timer;
mod tool_family;

mod whatwg;

fn write_source(ext: &str, body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile()
        .expect("tempfile");
    f.write_all(body.as_bytes()).expect("write");
    f
}

/// Final texts of chat-message entities, one per settled message.
/// Chat snapshots are the only ones carrying a `source` field.
fn chat_message_texts(frames: &[SectionTranscript]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|f| match f {
            SectionTranscript::Entity(crate::runtime::EntityCmd::Settle { snapshot, .. })
                if snapshot.get("source").is_some() =>
            {
                Some(snapshot["text"].as_str().unwrap_or_default().to_owned())
            }
            _ => None,
        })
        .collect()
}

fn text_of(delta: &SectionTranscript) -> String {
    match delta {
        SectionTranscript::Set { section: spec, .. } => match &spec.kind {
            SectionKind::Error => spec.seed.clone().unwrap_or_default(),
            SectionKind::ToolUse { name, detail } => match detail {
                Some(d) => format!("→ {name}  {d}"),
                None => format!("→ {name}"),
            },
            SectionKind::Json { tag, value } => format!("[{tag}] {value}"),
            SectionKind::Reasoning { state } => format!(
                "[reasoning:{state:?}]\n{}",
                spec.seed.clone().unwrap_or_default()
            ),
            SectionKind::Diff { lines } => format!("[diff:{} lines]", lines.len()),
            SectionKind::EntityRef { entity_id } => format!("[entity:{entity_id}]"),
        },
        SectionTranscript::Append { delta, .. } => delta.clone(),
        SectionTranscript::Close { id } => format!("[close:{}]", id.0),
        SectionTranscript::Entity(cmd) => format!("[entity:{cmd:?}]"),
    }
}
