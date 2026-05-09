use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error as ThisError;

use super::sse::{ToolCallDelta, ToolCallEvent};
use crate::provider::ToolCall;

#[derive(Debug, ThisError)]
pub enum ToolCallError {
    #[error("tool call at index {0} already started")]
    AlreadyStarted(u32),
    #[error("argument fragment for unstarted tool call at index {0}")]
    AppendBeforeStart(u32),
    #[error("parse arguments for tool call {id} ({name}): {source}")]
    ParseArguments {
        id: String,
        name: String,
        #[source]
        source: serde_json::Error,
    },
}

struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) struct ToolCallAccumulator {
    in_progress: BTreeMap<u32, ToolCallBuilder>,
}

impl ToolCallAccumulator {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push(&mut self, delta: ToolCallDelta<'_>) -> Result<(), ToolCallError> {
        match delta.event {
            ToolCallEvent::Start { id, name } => {
                if self.in_progress.contains_key(&delta.index) {
                    return Err(ToolCallError::AlreadyStarted(delta.index));
                }
                self.in_progress.insert(
                    delta.index,
                    ToolCallBuilder {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        arguments: String::new(),
                    },
                );
            }
            ToolCallEvent::Append(fragment) => {
                let builder = self
                    .in_progress
                    .get_mut(&delta.index)
                    .ok_or(ToolCallError::AppendBeforeStart(delta.index))?;
                builder.arguments.push_str(fragment);
            }
        }
        Ok(())
    }

    pub(super) fn finalize(self) -> Result<Vec<ToolCall>, ToolCallError> {
        self.in_progress
            .into_values()
            .map(|b| {
                let arguments: Value = serde_json::from_str(&b.arguments).map_err(|source| {
                    ToolCallError::ParseArguments {
                        id: b.id.clone(),
                        name: b.name.clone(),
                        source,
                    }
                })?;
                Ok(ToolCall {
                    id: b.id,
                    name: b.name,
                    arguments,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulator_end_to_end_single_call() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "call_1",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("{\"files\":"),
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("[]}"),
        })
        .unwrap();
        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].arguments, json!({"files": []}));
    }

    #[test]
    fn accumulator_two_parallel_calls_sorted_by_index() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 1,
            event: ToolCallEvent::Start {
                id: "b",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "a",
                name: "file_read",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("{}"),
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 1,
            event: ToolCallEvent::Append("{\"files\":[]}"),
        })
        .unwrap();
        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "a");
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].id, "b");
        assert_eq!(calls[1].name, "edit");
    }

    #[test]
    fn accumulator_rejects_append_before_start() {
        let mut acc = ToolCallAccumulator::new();
        let err = acc
            .push(ToolCallDelta {
                index: 0,
                event: ToolCallEvent::Append("{}"),
            })
            .unwrap_err();
        assert!(matches!(err, ToolCallError::AppendBeforeStart(0)));
    }

    #[test]
    fn accumulator_rejects_double_start() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "x",
                name: "edit",
            },
        })
        .unwrap();
        let err = acc
            .push(ToolCallDelta {
                index: 0,
                event: ToolCallEvent::Start {
                    id: "y",
                    name: "edit",
                },
            })
            .unwrap_err();
        assert!(matches!(err, ToolCallError::AlreadyStarted(0)));
    }

    #[test]
    fn accumulator_finalize_errors_on_malformed_arguments() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Start {
                id: "x",
                name: "edit",
            },
        })
        .unwrap();
        acc.push(ToolCallDelta {
            index: 0,
            event: ToolCallEvent::Append("not json"),
        })
        .unwrap();
        let err = acc.finalize().unwrap_err();
        assert!(matches!(err, ToolCallError::ParseArguments { .. }));
    }
}
