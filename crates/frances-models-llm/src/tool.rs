//! Tool-calling vocabulary sent to a provider: the tool definitions a model
//! may call and the choice mode that forces (or frees) calling. These are
//! frances's in-memory types; the `genai` provider maps them to its own
//! request types (`tool_def_to_genai`, `map_tool_choice`) — they are never
//! serialized directly.

use serde_json::Value;

/// A tool the model may call. Modelled on OpenAI's
/// `{"type":"function","function":{...}}` shape.
#[derive(Clone, Debug)]
pub enum ToolDef {
    Function(ToolFunction),
}

#[derive(Clone, Debug)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// How the model is allowed to call tools this turn. `Auto` (the default,
/// expressed by omitting a choice), `None` to forbid, `Required` to force any
/// tool, or `Function` to pin a specific one.
#[derive(Clone, Debug)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}
