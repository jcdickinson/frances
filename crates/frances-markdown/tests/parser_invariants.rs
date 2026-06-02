//! Exercises the `markdown` crate (markdown-rs) against the partial /
//! invalid input that streaming produces: an LLM completion arrives one
//! delta at a time, so the parser sees every prefix of the document —
//! including ones that cut a `**bold**` run in half or stop in the
//! middle of a fenced code block.
//!
//! The crate's contract is that plain markdown has no syntax errors, so
//! `to_mdast` always returns a tree. These tests pin that: every prefix
//! parses without erroring, and an unterminated fence still lands as a
//! `Code` node so the renderer can style it as code while it streams.

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

/// A doc with bold/italic in both flavours, an inline-code span, and a
/// fenced code block whose closing fence is the very last thing — so the
/// interesting prefixes are the ones with the fence still open.
const DOC: &str = "\
This is **bold** and *italic* with `inline` code.\n\
\n\
```rust\n\
fn main() {\n\
    println!(\"hi\");\n\
}\n\
```\n";

/// Walk the tree and return true if any node is a fenced/indented code
/// block (`mdast::Code`), regardless of how deeply it's nested.
fn has_code_block(node: &Node) -> bool {
    if matches!(node, Node::Code(_)) {
        return true;
    }
    node.children()
        .is_some_and(|kids| kids.iter().any(has_code_block))
}

/// Every prefix of the document parses without erroring. This is the
/// streaming guarantee: no matter where a delta boundary falls, the
/// parser hands back a usable tree rather than failing.
#[test]
fn every_prefix_parses_without_error() {
    let chars: Vec<char> = DOC.chars().collect();
    for n in 0..=chars.len() {
        let prefix: String = chars[..n].iter().collect();
        let parsed = to_mdast(&prefix, &ParseOptions::default());
        assert!(
            parsed.is_ok(),
            "prefix of length {n} failed to parse: {prefix:?}",
        );
    }
}

/// An unclosed fenced code block still registers as a `Code` node. While
/// the block streams in, the renderer needs to know it's inside code
/// before the closing ``` arrives.
#[test]
fn unclosed_code_fence_is_still_a_code_block() {
    let unclosed = "```rust\nfn main() {\n    println!(\"hi\");";
    let tree = to_mdast(unclosed, &ParseOptions::default()).expect("partial markdown never errors");
    assert!(
        has_code_block(&tree),
        "unterminated fence did not produce a Code node: {tree:?}",
    );
}

/// The content of an unclosed fence is preserved as the code body and
/// its language is captured — not dropped on the floor because the
/// closing fence hasn't arrived yet.
#[test]
fn unclosed_code_fence_keeps_lang_and_body() {
    let unclosed = "```rust\nlet x = 1;\nlet y = 2;";
    let tree = to_mdast(unclosed, &ParseOptions::default()).unwrap();
    let code = find_code(&tree).expect("expected a Code node");
    assert_eq!(code.lang.as_deref(), Some("rust"));
    assert!(
        code.value.contains("let x = 1;") && code.value.contains("let y = 2;"),
        "code body lost streamed lines: {:?}",
        code.value,
    );
}

fn find_code(node: &Node) -> Option<&markdown::mdast::Code> {
    if let Node::Code(code) = node {
        return Some(code);
    }
    node.children()?.iter().find_map(find_code)
}
