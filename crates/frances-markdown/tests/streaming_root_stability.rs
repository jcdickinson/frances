//! What can the incremental renderer trust about the root node list as a
//! document streams in?
//!
//! Streaming hands the parser every prefix of the document, one delta at
//! a time. The incremental renderer wants to commit earlier blocks once
//! and only rebuild the tail. Every test here drives off the single
//! `commonmark_all_nodes.md` fixture (every CommonMark node type, every
//! syntax) and pins one fact about streaming it. The invariants, all
//! asserted below:
//!
//! - **Committed (non-last) root node *types* are frozen.** Once a block
//!   is no longer the last root, its type never changes.
//! - **The root list never shrinks.** Block boundaries, once a blank line
//!   establishes them, are permanent — the count only grows.
//! - **Committed inline *content* is not frozen — but only a footer-link
//!   definition disturbs it.** When `[id]: url` becomes parseable, an
//!   earlier paragraph's `[text][id]` flips from plain text to a
//!   `LinkReference`. The paragraph's root *type* is unchanged; only its
//!   children are rewritten, and only on the step a definition lands.
//!
//! The *last* root node is deliberately **not** an invariant — its type is
//! not stable, so there is nothing to assert. As the rest of its line
//! arrives (and, for setext, the next line) the open tail block re-types:
//! `Paragraph` → `Heading` / `Code` / `List` / `ThematicBreak` / `Html`,
//! and `Paragraph` ⇄ `Definition`. A line's block type is genuinely
//! ambiguous until its terminator. So the renderer can trust the type and
//! count of everything *above* the open tail, but must treat the tail block
//! itself as provisional and rebuild it on each delta.

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

/// Root-level children of the parsed prefix. Partial markdown never
/// errors, so this always yields a tree.
fn roots(src: &str) -> Vec<Node> {
    match to_mdast(src, &ParseOptions::default()) {
        Ok(Node::Root(root)) => root.children,
        Ok(other) => vec![other],
        Err(e) => panic!("partial markdown should never error: {e:?}"),
    }
}

/// A node's type, ignoring its content — two `Paragraph`s with different
/// children compare equal here.
fn kind(node: &Node) -> std::mem::Discriminant<Node> {
    std::mem::discriminant(node)
}

/// Name of a root-level block type, for assertion messages. Root nodes in
/// CommonMark are only ever one of these eight.
fn type_name(node: &Node) -> &'static str {
    match node {
        Node::Paragraph(_) => "Paragraph",
        Node::Heading(_) => "Heading",
        Node::Code(_) => "Code",
        Node::Blockquote(_) => "Blockquote",
        Node::List(_) => "List",
        Node::ThematicBreak(_) => "ThematicBreak",
        Node::Definition(_) => "Definition",
        Node::Html(_) => "Html",
        _ => "Other",
    }
}

fn definition_count(roots: &[Node]) -> usize {
    roots
        .iter()
        .filter(|n| matches!(n, Node::Definition(_)))
        .count()
}

const ALL_NODES: &str = include_str!("fixtures/commonmark_all_nodes.md");

/// Walk every prefix of the fixture, calling `step(prefix_len, prev, cur)`
/// for each consecutive pair of parses.
fn for_each_prefix_step(mut step: impl FnMut(usize, &[Node], &[Node])) {
    let chars: Vec<char> = ALL_NODES.chars().collect();
    let mut prev = roots("");
    for n in 1..=chars.len() {
        let prefix: String = chars[..n].iter().collect();
        let cur = roots(&prefix);
        step(n, &prev, &cur);
        prev = cur;
    }
}

/// Once a root node is no longer the last one, its type is fixed. (This is
/// the original worry: an earlier `Paragraph` silently becoming a `Code`
/// block, etc. It does not happen — indented code blocks included.)
#[test]
fn committed_root_node_types_are_frozen() {
    for_each_prefix_step(|n, prev, cur| {
        if prev.is_empty() {
            return;
        }
        for (i, node) in prev[..prev.len() - 1].iter().enumerate() {
            assert_eq!(
                cur.get(i).map(kind),
                Some(kind(node)),
                "committed root node #{i} ({}) changed type at prefix len {n} — \
                 an earlier block was supposed to be frozen",
                type_name(node),
            );
        }
    });
}

/// The root list only ever grows. A blank line that separates two blocks
/// is a permanent boundary, so no prefix produces fewer root nodes than
/// its predecessor.
#[test]
fn root_list_never_shrinks() {
    for_each_prefix_step(|n, prev, cur| {
        assert!(
            cur.len() >= prev.len(),
            "root count shrank at prefix len {n}: {} -> {}",
            prev.len(),
            cur.len(),
        );
    });
}

/// Why indented code blocks don't break the frozen-type invariant: the
/// spec's interior-blank case (`    foo` / blank / `    bar`) is absorbed
/// into one `Code` node rather than splitting at the blank. The fixture's
/// second indented block has exactly that interior blank.
#[test]
fn interior_blank_keeps_indented_code_a_single_node() {
    let has_interior_blank_code = roots(ALL_NODES)
        .iter()
        .any(|n| matches!(n, Node::Code(c) if c.value.contains("\n\n")));
    assert!(
        has_interior_blank_code,
        "expected an indented code block whose interior blank line was absorbed",
    );
}

/// The one thing that reaches back into an already-committed node: a
/// trailing footer-link definition. Every step that disturbs an earlier
/// node coincides with the document's definition count changing — and at
/// least one such step exists. (Combined with the frozen-type test, this
/// says earlier nodes change *only* in inline content, *only* when a
/// definition lands.)
#[test]
fn committed_nodes_change_only_when_a_definition_lands() {
    let mut disturbing_steps = 0usize;
    for_each_prefix_step(|n, prev, cur| {
        if prev.is_empty() {
            return;
        }
        let earlier_changed = prev[..prev.len() - 1]
            .iter()
            .enumerate()
            .any(|(i, node)| cur.get(i) != Some(node));
        if earlier_changed {
            disturbing_steps += 1;
            assert_ne!(
                definition_count(prev),
                definition_count(cur),
                "a committed root node changed at prefix len {n} without a \
                 definition landing — an unexpected retroactive edit",
            );
        }
    });
    assert!(
        disturbing_steps > 0,
        "expected footer-link definitions to retroactively rewrite earlier paragraphs",
    );
}
