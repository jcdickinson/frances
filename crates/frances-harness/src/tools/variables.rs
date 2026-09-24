use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx as JaqCtx, Vars, data, unwrap_valr};
use jaq_json::Val;

fn eval_filter(filter: &str, input_json: &str, bindings_json: &str) -> Result<String, FilterError> {
    let input: Val = jaq_json::read::parse_single(input_json.as_bytes())
        .map_err(|e| FilterError::Input(e.to_string()))?;

    let bindings = parse_bindings(bindings_json)?;
    // jaq matches global-variable names verbatim against what the
    // parser sees in the filter (e.g. `$foo` → name "$foo"), so we
    // declare bindings with the leading `$` baked in.
    let (var_names, var_values): (Vec<String>, Vec<Val>) = bindings
        .into_iter()
        .map(|(name, val)| (format!("${name}"), val))
        .unzip();

    let program = File {
        code: filter,
        path: (),
    };

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errs| FilterError::Compile(format!("{errs:?}")))?;

    let filter = Compiler::default()
        .with_funs(funs)
        .with_global_vars(var_names.iter().map(String::as_str))
        .compile(modules)
        .map_err(|errs| FilterError::Compile(format!("{errs:?}")))?;

    let jaq_ctx = JaqCtx::<data::JustLut<Val>>::new(&filter.lut, Vars::new(var_values));
    let mut outputs = filter.id.run((jaq_ctx, input)).map(unwrap_valr);

    let first = outputs
        .next()
        .ok_or(FilterError::NoOutput)?
        .map_err(|e| FilterError::Run(e.to_string()))?;

    if outputs.next().is_some() {
        return Err(FilterError::MultipleOutputs);
    }

    Ok(first.to_string())
}

/// Parse the bindings JSON into an ordered list of (name, Val) pairs.
/// `with_global_vars` and `Vars::new` must receive names and values in
/// the same order; only consistency matters, not the order itself.
fn parse_bindings(bindings_json: &str) -> Result<Vec<(String, Val)>, FilterError> {
    let parsed: serde_json::Value =
        serde_json::from_str(bindings_json).map_err(|e| FilterError::Input(e.to_string()))?;
    let serde_json::Value::Object(map) = parsed else {
        return Err(FilterError::Bindings);
    };
    let mut out = Vec::with_capacity(map.len());
    for (name, value) in map {
        let encoded = serde_json::to_vec(&value).map_err(|e| FilterError::Input(e.to_string()))?;
        let val: Val = jaq_json::read::parse_single(&encoded)
            .map_err(|e| FilterError::Input(format!("{name}: {e}")))?;
        out.push((name, val));
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("invalid filter input: {0}")]
    Input(String),
    #[error("filter compilation failed: {0}")]
    Compile(String),
    #[error("filter execution failed: {0}")]
    Run(String),
    #[error("filter produced no output")]
    NoOutput,
    #[error("filter produced multiple outputs; collect them in an array")]
    MultipleOutputs,
    #[error("bindings must be an object")]
    Bindings,
}

use super::{ToolError, Tools, string};
use crate::HarnessDeps;
use frances_models_llm::ToolCall;
use serde_json::{Map, Value};

impl<D: HarnessDeps> Tools<D> {
    pub(super) fn variable_call(&mut self, call: &ToolCall) -> Result<String, ToolError> {
        let args = &call.arguments;
        let name = string(args, "name")?;
        match call.name.as_str() {
            "var_set" => {
                let value = args
                    .get("value")
                    .ok_or_else(|| ToolError::InvalidArguments("value is required".into()))?;
                self.variables.insert(name.into(), value.clone());
                Ok(describe(name, value))
            }
            "var_get" => {
                let value = self
                    .variables
                    .get(name)
                    .ok_or_else(|| ToolError::Variable(name.into()))?;
                if let Some(filter) = args.get("filter").and_then(Value::as_str) {
                    Ok(eval_filter(filter, &value.to_string(), "{}")?)
                } else {
                    Ok(serde_json::to_string_pretty(value)?)
                }
            }
            _ => {
                let filter = string(args, "filter")?;
                let inputs: Vec<String> = args
                    .get("inputs")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let mut bindings = Map::new();
                for input in inputs {
                    let value = self
                        .variables
                        .get(&input)
                        .ok_or_else(|| ToolError::Variable(input.clone()))?;
                    bindings.insert(input, value.clone());
                }
                let value = self.variables.get(name).unwrap_or(&Value::Null);
                let result = eval_filter(
                    filter,
                    &value.to_string(),
                    &Value::Object(bindings).to_string(),
                )?;
                self.variables
                    .insert(name.into(), serde_json::from_str(&result)?);
                Ok(describe(name, &self.variables[name]))
            }
        }
    }
}

fn describe(name: &str, value: &Value) -> String {
    let shape = match value {
        Value::Null => "null".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
        Value::Array(items) => format!("array({} items)", items.len()),
        Value::Object(items) => format!("object({} keys)", items.len()),
    };
    format!("{name} = {shape}")
}
