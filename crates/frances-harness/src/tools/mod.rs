mod definitions;
pub use definitions::definitions;
pub mod file;
pub mod search;
mod shell;
mod variables;

use crate::deps::EditorSession;
use crate::io::{HarnessShell, HarnessShellHandle};
use crate::{EditorFactory, HarnessDeps, Output, Outputs};
use frances_models_llm::{ToolCall, ToolDef};
use frances_models_ui::SectionKind;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("invalid tool arguments: {0}")]
    Arguments(#[from] serde_json::Error),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error(transparent)]
    File(#[from] file::FileToolError),
    #[error(transparent)]
    Search(#[from] search::SearchError),
    #[error(transparent)]
    Shell(#[from] frances_shell::ShellError),
    #[error(transparent)]
    Filter(#[from] variables::FilterError),
    #[error("unknown variable: {0}")]
    Variable(String),
    #[error("tool call interrupted")]
    Interrupted,
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("no shell command is running")]
    NoShell,
}

/// One context's native tools, editor read cache, variables, and shell state.
/// Definitions are constructed once and remain fixed for its lifetime.
pub struct Tools<D: HarnessDeps> {
    deps: D,
    editor: EditorSession<D>,
    definitions: Vec<ToolDef>,
    shell: Option<<D::Shell as HarnessShell>::Handle>,
    shell_view: Option<shell::ShellView>,
    variables: HashMap<String, Value>,
    output: Outputs,
}

impl<D: HarnessDeps> Tools<D> {
    pub fn new(deps: D, output: Outputs) -> Self {
        Self {
            editor: Arc::new(tokio::sync::Mutex::new(deps.editor_factory().new_session())),
            definitions: definitions::definitions(),
            deps,
            shell: None,
            shell_view: None,
            variables: HashMap::new(),
            output,
        }
    }
    pub fn definitions(&self) -> &[ToolDef] {
        &self.definitions
    }

    pub async fn execute(
        &mut self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Interrupted);
        }
        let Some(ToolDef::Function(definition)) = self
            .definitions
            .iter()
            .find(|ToolDef::Function(f)| f.name == call.name)
        else {
            return Err(ToolError::Unknown(call.name.clone()));
        };
        frances_models_llm::tool_args::validate(&call.arguments, &definition.parameters)
            .map_err(|error| ToolError::InvalidArguments(error.0))?;
        if let Some(error) = &call.error {
            return Err(ToolError::InvalidArguments(error.message.clone()));
        }
        match call.name.as_str() {
            "file_read" => {
                let args = &call.arguments;
                if let Some(into) = args.get("into").and_then(Value::as_str) {
                    if args.get("ranges").is_some_and(|v| !v.is_null()) {
                        return Err(ToolError::InvalidArguments(
                            "into and ranges are mutually exclusive".into(),
                        ));
                    }
                    let text = file::read_raw_inner(
                        &self.deps,
                        &self.editor,
                        string(args, "path")?.into(),
                    )
                    .await?;
                    self.variables.insert(into.into(), Value::String(text));
                    return Ok(format!("{into} = string"));
                }
                let read: file::ReadFileArgs = serde_json::from_value(args.clone())?;
                let text = file::read_file_inner(&self.deps, &self.editor, read).await?;
                let snapshot = file_snapshot(args, &text);
                let id = self.output.open("file", snapshot.clone());
                self.output.settle(id, snapshot);
                Ok(text)
            }
            "file_find_or_grep" => {
                let mut args = call.arguments.clone();
                let into = args.as_object_mut().and_then(|a| a.remove("into"));
                let result = search::search_inner(&self.deps, &self.editor, args).await?;
                if let Some(Value::String(name)) = into {
                    self.variables.insert(name, serde_json::from_str(&result)?);
                }
                Ok(result)
            }
            "file_replace_lines" | "file_replace_all" | "file_insert_after"
            | "file_insert_before" | "file_new" | "file_overwrite" => {
                let mut args = call.arguments.clone();
                let kind = match call.name.as_str() {
                    "file_replace_lines" => "ReplaceLines",
                    "file_replace_all" => "ReplaceAll",
                    "file_insert_after" => "InsertAfter",
                    "file_insert_before" => "InsertBefore",
                    "file_new" => "New",
                    _ => "Overwrite",
                };
                if kind != "ReplaceAll" {
                    let text = self.resolve_text(&args)?;
                    args["text"] = text.into();
                }
                args["kind"] = kind.into();
                let diff = file::edit_inner(&self.deps, &self.editor, args).await?;
                if !diff.ops.is_empty() {
                    self.output
                        .send(Output::Section(SectionKind::Diff { lines: diff.ops }));
                }
                Ok(diff.text)
            }
            "shell_run" | "shell_wait" | "shell_kill" => self.shell_call(call, cancel).await,
            "var_set" | "var_get" | "var_edit" => self.variable_call(call),
            "shell_set" => {
                let name = string(&call.arguments, "name")?.to_owned();
                let from = string(&call.arguments, "from")?;
                let value = self
                    .variables
                    .get(from)
                    .ok_or_else(|| ToolError::Variable(from.into()))?;
                let value = render_value(value).into_bytes();
                self.ensure_shell().await?;
                validate_shell_name(&name)?;
                self.shell
                    .as_mut()
                    .expect("shell initialized")
                    .set_var(name.clone(), value)
                    .await?;
                Ok(format!("exported ${name}"))
            }
            "shell_get" => {
                let name = string(&call.arguments, "name")?.to_owned();
                let from = string(&call.arguments, "from")?.to_owned();
                self.ensure_shell().await?;
                validate_shell_name(&from)?;
                let value = self
                    .shell
                    .as_mut()
                    .expect("shell initialized")
                    .get_var(from)
                    .await?;
                self.variables.insert(name.clone(), value.into());
                Ok(format!("{name} = string"))
            }
            _ => Err(ToolError::Unknown(call.name.clone())),
        }
    }
    fn resolve_text(&self, args: &Value) -> Result<String, ToolError> {
        match (
            args.get("text").and_then(Value::as_str),
            args.get("from").and_then(Value::as_str),
        ) {
            (Some(text), None) => Ok(text.into()),
            (None, Some(from)) => self
                .variables
                .get(from)
                .map(render_value)
                .ok_or_else(|| ToolError::Variable(from.into())),
            _ => Err(ToolError::InvalidArguments(
                "provide exactly one of text or from".into(),
            )),
        }
    }
    pub async fn commit_edits(&mut self) -> Result<(), ToolError> {
        Ok(file::commit_inner::<D>(&self.editor).await?)
    }
}

fn string<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments(format!("{name} must be a string")))
}
fn render_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn file_snapshot(args: &Value, text: &str) -> Value {
    let ranges: Vec<[usize; 2]> = args
        .get("ranges")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .expect("read arguments were validated")
        .unwrap_or_else(|| vec![[1, usize::MAX]]);
    let ranges = file::merge_ranges(&ranges, usize::MAX).expect("read ranges were validated");
    let mut block = 0;
    let mut line = ranges.first().map_or(1, |range| range[0]);
    let mut rows = Vec::new();
    for raw in text.lines() {
        if raw == "…§" || raw == "…" {
            rows.push(json!({"kind":"gap"}));
            block += 1;
            line = ranges.get(block).map_or(1, |range| range[0]);
            continue;
        }
        if let Some((number, text)) = raw.split_once(':')
            && let Ok(number) = number.parse::<usize>()
        {
            rows.push(json!({"kind":"line", "line":number, "anchor":null, "text":text}));
            continue;
        }
        if let Some((anchor, text)) = raw.split_once('§') {
            rows.push(json!({"kind":"line", "line":line, "anchor":anchor, "text":text}));
            line += 1;
        }
    }
    json!({"path":args["path"], "rows":rows})
}

fn validate_shell_name(name: &str) -> Result<(), ToolError> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        || name == "FRANCES_ROOT"
    {
        return Err(ToolError::InvalidArguments(
            "expected a non-reserved bash variable name".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
