use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error as ThisError;

use super::sse::{ToolCallDelta, ToolCallEvent};
use frances_models_llm::wire::ToolCall;

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

impl ToolCallBuilder {
    fn finalize(self) -> Result<ToolCall, ToolCallError> {
        let arguments: Value = serde_json::from_str(&self.arguments).map_err(|source| {
            ToolCallError::ParseArguments {
                id: self.id.clone(),
                name: self.name.clone(),
                source,
            }
        })?;
        Ok(ToolCall {
            id: self.id,
            name: self.name,
            arguments,
        })
    }
}

#[derive(Default)]
pub(super) struct ToolCallAccumulator {
    in_progress: BTreeMap<u32, ToolCallBuilder>,
    /// Highest `Start` index seen so far. Used to detect a *new* call
    /// starting: a `Start { index: N }` where `N > max_started` lets us
    /// presume that every entry currently in `in_progress` with index
    /// `< N` is complete (real OpenAI-style providers serialise tool
    /// calls in increasing index order, so prior indices won't receive
    /// any further `Append`s).
    max_started: Option<u32>,
}

impl ToolCallAccumulator {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Apply one delta. Returns the (possibly empty) list of tool calls
    /// that just became eligible to emit as `StreamEvent::ToolCall`. The
    /// caller is responsible for emitting them in returned order.
    pub(super) fn push(
        &mut self,
        delta: ToolCallDelta<'_>,
    ) -> Result<Vec<ToolCall>, ToolCallError> {
        match delta.event {
            ToolCallEvent::Start { id, name } => {
                if self.in_progress.contains_key(&delta.index) {
                    return Err(ToolCallError::AlreadyStarted(delta.index));
                }
                let mut emit: Vec<ToolCall> = Vec::new();
                if self.max_started.is_none_or(|prev| delta.index > prev) {
                    // Monotonic advance: everything strictly below this
                    // index is now provably done. `split_off(&index)`
                    // leaves keys `< index` in `self.in_progress` and
                    // returns keys `>= index` — so we swap and finalize
                    // the lower half.
                    let upper = self.in_progress.split_off(&delta.index);
                    let lower = std::mem::replace(&mut self.in_progress, upper);
                    for builder in lower.into_values() {
                        emit.push(builder.finalize()?);
                    }
                    self.max_started = Some(delta.index);
                }
                self.in_progress.insert(
                    delta.index,
                    ToolCallBuilder {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        arguments: String::new(),
                    },
                );
                Ok(emit)
            }
            ToolCallEvent::Append(fragment) => {
                let builder = self
                    .in_progress
                    .get_mut(&delta.index)
                    .ok_or(ToolCallError::AppendBeforeStart(delta.index))?;
                builder.arguments.push_str(fragment);
                Ok(Vec::new())
            }
        }
    }

    /// Drain everything still in flight. Called once at end-of-stream
    /// for the trailing call(s) the eager-emit path never released.
    pub(super) fn finalize(self) -> Result<Vec<ToolCall>, ToolCallError> {
        self.in_progress
            .into_values()
            .map(ToolCallBuilder::finalize)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn start(index: u32, id: &'static str, name: &'static str) -> ToolCallDelta<'static> {
        ToolCallDelta {
            index,
            event: ToolCallEvent::Start { id, name },
        }
    }

    fn append(index: u32, fragment: &'static str) -> ToolCallDelta<'static> {
        ToolCallDelta {
            index,
            event: ToolCallEvent::Append(fragment),
        }
    }

    #[test]
    fn accumulator_end_to_end_single_call() {
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.push(start(0, "call_1", "edit")).unwrap().is_empty());
        assert!(acc.push(append(0, "{\"files\":")).unwrap().is_empty());
        assert!(acc.push(append(0, "[]}")).unwrap().is_empty());
        let calls = acc.finalize().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].arguments, json!({"files": []}));
    }

    #[test]
    fn accumulator_serial_starts_emit_eagerly() {
        // Real-provider shape: index N's args arrive before Start(N+1).
        // Start(N+1) is the signal that N is done; the accumulator should
        // hand back the finalised N at that point and let the SSE loop
        // emit `StreamEvent::ToolCall` immediately.
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.push(start(0, "a", "read")).unwrap().is_empty());
        assert!(acc.push(append(0, "{}")).unwrap().is_empty());

        let emitted = acc.push(start(1, "b", "write")).unwrap();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].id, "a");
        assert_eq!(emitted[0].name, "read");
        assert_eq!(emitted[0].arguments, json!({}));

        assert!(acc.push(append(1, "{\"x\":1}")).unwrap().is_empty());
        let trailing = acc.finalize().unwrap();
        assert_eq!(trailing.len(), 1, "only the last call should remain");
        assert_eq!(trailing[0].id, "b");
        assert_eq!(trailing[0].arguments, json!({"x": 1}));
    }

    #[test]
    fn accumulator_out_of_order_starts_batch_at_finalize() {
        // Synthetic case: Start(1) arrives before Start(0). No real
        // provider does this today, but the BTreeMap guarantees we
        // still finalise both in index order at end-of-stream. The
        // monotonic-advance guard means the out-of-order Start does
        // NOT trigger a (premature) finalisation of higher-indexed
        // calls.
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.push(start(1, "b", "edit")).unwrap().is_empty());
        assert!(
            acc.push(start(0, "a", "file_read")).unwrap().is_empty(),
            "Start(0) after Start(1) is out-of-order; nothing eager-emits",
        );
        assert!(acc.push(append(0, "{}")).unwrap().is_empty());
        assert!(acc.push(append(1, "{\"files\":[]}")).unwrap().is_empty());

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
        let err = acc.push(append(0, "{}")).unwrap_err();
        assert!(matches!(err, ToolCallError::AppendBeforeStart(0)));
    }

    #[test]
    fn accumulator_rejects_double_start() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(start(0, "x", "edit")).unwrap();
        let err = acc.push(start(0, "y", "edit")).unwrap_err();
        assert!(matches!(err, ToolCallError::AlreadyStarted(0)));
    }

    #[test]
    fn accumulator_finalize_errors_on_malformed_arguments() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(start(0, "x", "edit")).unwrap();
        acc.push(append(0, "not json")).unwrap();
        let err = acc.finalize().unwrap_err();
        assert!(matches!(err, ToolCallError::ParseArguments { .. }));
    }

    #[test]
    fn accumulator_eager_emit_surfaces_argument_parse_errors() {
        // If the call we eagerly finalise has malformed JSON args, the
        // error propagates out of `push`, not just `finalize`.
        let mut acc = ToolCallAccumulator::new();
        acc.push(start(0, "x", "edit")).unwrap();
        acc.push(append(0, "not json")).unwrap();
        let err = acc.push(start(1, "y", "edit")).unwrap_err();
        assert!(matches!(err, ToolCallError::ParseArguments { .. }));
    }
}
