use serde_json::Value;

use frances_models_llm::wire::Usage;

#[derive(Debug, Clone, Copy)]
pub(super) struct ToolCallDelta<'a> {
    pub(super) index: u32,
    pub(super) event: ToolCallEvent<'a>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ToolCallEvent<'a> {
    Start { id: &'a str, name: &'a str },
    Append(&'a str),
}

pub(super) fn chunk_text_deltas(chunk: &Value) -> impl Iterator<Item = &str> {
    chunk
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
        })
        .filter(|text| !text.is_empty())
}

pub(super) fn chunk_tool_call_deltas(chunk: &Value) -> Vec<ToolCallDelta<'_>> {
    let mut out = Vec::new();
    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
        return out;
    };
    for choice in choices {
        let Some(tcs) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for tc in tcs {
            let Some(index) = tc.get("index").and_then(Value::as_u64).map(|n| n as u32) else {
                continue;
            };
            let id = tc.get("id").and_then(Value::as_str);
            let function = tc.get("function");
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str);
            let fragment = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str);

            if let (Some(id), Some(name)) = (id, name) {
                out.push(ToolCallDelta {
                    index,
                    event: ToolCallEvent::Start { id, name },
                });
            }
            if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
                out.push(ToolCallDelta {
                    index,
                    event: ToolCallEvent::Append(fragment),
                });
            }
        }
    }
    out
}

pub(super) fn chunk_usage(chunk: &Value) -> Option<Usage> {
    let usage = chunk.get("usage")?;
    if usage.is_null() {
        return None;
    }
    Some(Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        completion_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        cached_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_delta_chunk_yields_text() {
        let chunk = json!({
            "choices": [{ "delta": { "content": "hello" } }]
        });
        let texts: Vec<&str> = chunk_text_deltas(&chunk).collect();
        assert_eq!(texts, vec!["hello"]);
    }

    #[test]
    fn text_delta_skips_empty() {
        let chunk = json!({
            "choices": [{ "delta": { "content": "" } }]
        });
        let texts: Vec<&str> = chunk_text_deltas(&chunk).collect();
        assert!(texts.is_empty());
    }

    #[test]
    fn first_chunk_yields_start_event_only() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "edit", "arguments": "" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].index, 0);
        match deltas[0].event {
            ToolCallEvent::Start { id, name } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "edit");
            }
            _ => panic!("expected Start event"),
        }
    }

    #[test]
    fn argument_fragment_yields_append_event() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "{\"files\":" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 1);
        match deltas[0].event {
            ToolCallEvent::Append(frag) => assert_eq!(frag, "{\"files\":"),
            _ => panic!("expected Append event"),
        }
    }

    #[test]
    fn chunk_with_start_and_nonempty_args_yields_two_events() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "x",
                        "function": { "name": "edit", "arguments": "{}" }
                    }]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 2);
        assert!(matches!(deltas[0].event, ToolCallEvent::Start { .. }));
        assert!(matches!(deltas[1].event, ToolCallEvent::Append("{}")));
    }

    #[test]
    fn parallel_calls_yield_distinct_indices() {
        let chunk = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "a", "function": {"name": "file_read"}},
                        {"index": 1, "id": "b", "function": {"name": "edit"}}
                    ]
                }
            }]
        });
        let deltas = chunk_tool_call_deltas(&chunk);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].index, 0);
        assert_eq!(deltas[1].index, 1);
    }
}
