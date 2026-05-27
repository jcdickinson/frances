use std::path::Path;

use deno_ast::{MediaType, ModuleSpecifier, ParseParams, SourceMapOption, TranspileOptions};

use crate::WorkflowError;

/// Source kind, derived from the file extension. Anything unknown is
/// treated as plain JS — QuickJS will surface a parse error if it's not
/// actually JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    JavaScript,
    TypeScript,
}

impl SourceKind {
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|s| s.to_str()) {
            Some("ts") | Some("mts") => Self::TypeScript,
            _ => Self::JavaScript,
        }
    }
}

/// Strip TypeScript types via deno_ast's swc-backed transpile. No
/// bundling, no module resolution — single file in, plain JS string out.
pub(crate) fn ts_to_js(path: &Path, source: &str) -> Result<String, WorkflowError> {
    // The specifier has to look like a URL; ModuleSpecifier::from_file_path
    // demands an absolute path, so canonicalize-or-fall-back-to-as-given.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(WorkflowError::ReadSource)?
            .join(path)
    };
    let specifier = ModuleSpecifier::from_file_path(&absolute)
        .map_err(|()| WorkflowError::TranspileSpecifier(absolute.clone()))?;

    let parsed = deno_ast::parse_module(ParseParams {
        specifier,
        text: source.into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })?;

    let transpiled = parsed.transpile(
        &TranspileOptions::default(),
        &deno_ast::TranspileModuleOptions::default(),
        &deno_ast::EmitOptions {
            source_map: SourceMapOption::None,
            ..Default::default()
        },
    )?;

    Ok(transpiled.into_source().text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped default workflow is a TS asset that nothing else in
    /// the build compiles. Transpiling it here catches syntax/type-strip
    /// regressions (e.g. when editing `referee` to use `complete`).
    #[test]
    fn default_workflow_asset_transpiles() {
        let src = include_str!("../../../assets/workflows/main.ts");
        ts_to_js(Path::new("main.ts"), src)
            .expect("assets/workflows/main.ts should transpile cleanly");
    }
}
