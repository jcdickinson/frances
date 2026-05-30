//! `_jaqEval` stash entry — synchronous jq filter evaluator.
//!
//! Exposed to JS as `_jaqEval(filter, inputJson, bindingsJson) -> string`.
//! All three arguments are strings: the caller `JSON.stringify`s the
//! input value and the bindings map (`{ name: value }`), and parses the
//! returned string back with `JSON.parse`. Going through JSON on the FFI
//! boundary sidesteps the rquickjs↔serde↔jaq value-type web entirely.
//!
//! Bindings populate jq's `$name` global-variable surface. The compiler
//! is told the variable names; `Vars::new` feeds the matching values in
//! the same order at run time.
//!
//! The filter must produce exactly one output value. Zero outputs and
//! multi-output filters both error — the caller wraps with `[...]` if
//! it wants an array.
//!
//! Compilation is a per-call cost (no caching). jq filters are tiny and
//! jaq's compiler is fast, so this hasn't been worth the complexity yet.

use rquickjs::{Ctx, Function, Result as JsResult};

use super::throw_js as throw;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx as JaqCtx, Vars, data, unwrap_valr};
use jaq_json::Val;

pub(crate) fn build_jaq_eval<'js>(ctx: &Ctx<'js>) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         filter: String,
         input_json: String,
         bindings_json: String|
         -> JsResult<String> {
            match eval_filter(&filter, &input_json, &bindings_json) {
                Ok(out) => Ok(out),
                Err(msg) => Err(throw(&ctx, &msg)),
            }
        },
    )
}

/// JSON-in / JSON-out filter eval. `bindings_json` must encode a flat
/// object (`{ name: value, ... }`); each entry becomes a jq global
/// variable. The destination's prior value (or `null`) is the `.` input.
fn eval_filter(filter: &str, input_json: &str, bindings_json: &str) -> Result<String, String> {
    let input: Val = jaq_json::read::parse_single(input_json.as_bytes())
        .map_err(|e| format!("parse input: {e}"))?;

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
        .map_err(|errs| format!("load: {errs:?}"))?;

    let filter = Compiler::default()
        .with_funs(funs)
        .with_global_vars(var_names.iter().map(String::as_str))
        .compile(modules)
        .map_err(|errs| format!("compile: {errs:?}"))?;

    let jaq_ctx = JaqCtx::<data::JustLut<Val>>::new(&filter.lut, Vars::new(var_values));
    let mut outputs = filter.id.run((jaq_ctx, input)).map(unwrap_valr);

    let first = outputs
        .next()
        .ok_or_else(|| "filter produced no output".to_owned())?
        .map_err(|e| format!("run: {e}"))?;

    if outputs.next().is_some() {
        return Err(
            "filter produced multiple outputs; wrap with `[...]` to collect into an array"
                .to_owned(),
        );
    }

    Ok(first.to_string())
}

/// Parse the bindings JSON into an ordered list of (name, Val) pairs.
/// Ordering doesn't matter for correctness — `with_global_vars` and
/// `Vars::new` only need consistent order between them — but using a
/// `Vec` keeps the iteration deterministic.
fn parse_bindings(bindings_json: &str) -> Result<Vec<(String, Val)>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(bindings_json).map_err(|e| format!("parse bindings: {e}"))?;
    let serde_json::Value::Object(map) = parsed else {
        return Err("bindings must be a JSON object".to_owned());
    };
    let mut out = Vec::with_capacity(map.len());
    for (name, value) in map {
        let encoded = serde_json::to_vec(&value).map_err(|e| format!("re-encode binding: {e}"))?;
        let val: Val = jaq_json::read::parse_single(&encoded)
            .map_err(|e| format!("parse binding {name}: {e}"))?;
        out.push((name, val));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_identity_filter() {
        let out = eval_filter(".", "42", "{}").unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn evaluates_with_global_var() {
        let out = eval_filter("$a + $b", "null", r#"{"a":2,"b":3}"#).unwrap();
        assert_eq!(out, "5");
    }

    #[test]
    fn keys_introspection_works() {
        let out = eval_filter("$o | keys", "null", r#"{"o":{"x":1,"y":2}}"#).unwrap();
        assert_eq!(out, r#"["x","y"]"#);
    }

    #[test]
    fn fromjson_parses_string() {
        let out = eval_filter("fromjson", "\"[1,2,3]\"", "{}").unwrap();
        assert_eq!(out, "[1,2,3]");
    }

    #[test]
    fn errors_on_zero_outputs() {
        let err = eval_filter("empty", "null", "{}").unwrap_err();
        assert!(err.contains("no output"), "got: {err}");
    }

    #[test]
    fn errors_on_multiple_outputs() {
        let err = eval_filter("1, 2, 3", "null", "{}").unwrap_err();
        assert!(err.contains("multiple outputs"), "got: {err}");
    }
}
