use std::fmt::Write;

use similar::{Algorithm, DiffTag, capture_diff_slices};

use crate::anchor::Anchor;
use crate::state::FileAnchorState;

pub const ANCHOR_SEP: char = '§';

/// Renders the file with one line per source line as `anchor§content`.
/// The rendered string of each line is what the model passes back as the
/// `anchor` field of a structured edit.
pub fn render_file(state: &FileAnchorState, lines: &[String]) -> String {
    let mut out = String::with_capacity(lines.len() * 16);
    for (le, line) in state.lines.iter().zip(lines) {
        write!(out, "{}{ANCHOR_SEP}{line}", le.anchor).expect("write to String");
        out.push('\n');
    }
    out
}

/// Render a unified-diff-style block summarizing the change from
/// (`pre_state`, `pre_lines`) to (`post_state`, `post_lines`). The diff is
/// computed over anchor identities — anchors are unique per file, so this
/// produces the correct alignment even when multiple lines share content.
///
/// Lines are emitted as:
///   ` Anchor§content`  — context (carried anchor)
///   `-Anchor§content`  — tombstoned (anchor in pre, gone from post)
///   `+Anchor§content`  — minted (new anchor in post)
///
/// `context` controls how many lines on either side of a change region are
/// included as context. Long unchanged stretches between change regions are
/// truncated, separated by a blank line.
pub fn render_diff_block(
    pre_state: &FileAnchorState,
    pre_lines: &[String],
    post_state: &FileAnchorState,
    post_lines: &[String],
    context: usize,
) -> String {
    let pre_anchors: Vec<&Anchor> = pre_state.lines.iter().map(|le| &le.anchor).collect();
    let post_anchors: Vec<&Anchor> = post_state.lines.iter().map(|le| &le.anchor).collect();
    let ops = capture_diff_slices(Algorithm::Patience, &pre_anchors, &post_anchors);

    let mut out = String::new();

    for (op_idx, op) in ops.iter().enumerate() {
        let is_first = op_idx == 0;
        let is_last = op_idx == ops.len() - 1;

        match op.tag() {
            DiffTag::Equal => {
                let new_range = op.new_range();
                let len = new_range.len();
                let take_start = if is_first { 0 } else { context.min(len) };
                let take_end = if is_last { 0 } else { context.min(len) };

                if take_start + take_end >= len {
                    for i in new_range.clone() {
                        emit_line(&mut out, ' ', post_anchors[i], &post_lines[i], i + 1);
                    }
                } else {
                    for i in 0..take_start {
                        let idx = new_range.start + i;
                        emit_line(&mut out, ' ', post_anchors[idx], &post_lines[idx], idx + 1);
                    }
                    if take_start > 0 {
                        out.push('\n');
                    }
                    for i in 0..take_end {
                        let idx = new_range.end - take_end + i;
                        emit_line(&mut out, ' ', post_anchors[idx], &post_lines[idx], idx + 1);
                    }
                }
            }
            DiffTag::Delete => {
                for i in op.old_range() {
                    emit_line(&mut out, '-', pre_anchors[i], &pre_lines[i], i + 1);
                }
            }
            DiffTag::Insert => {
                for i in op.new_range() {
                    emit_line(&mut out, '+', post_anchors[i], &post_lines[i], i + 1);
                }
            }
            DiffTag::Replace => {
                for i in op.old_range() {
                    emit_line(&mut out, '-', pre_anchors[i], &pre_lines[i], i + 1);
                }
                for i in op.new_range() {
                    emit_line(&mut out, '+', post_anchors[i], &post_lines[i], i + 1);
                }
            }
        }
    }

    out
}

fn emit_line(out: &mut String, prefix: char, anchor: &Anchor, content: &str, line: usize) {
    write!(out, "{prefix} {line:4} {anchor}{ANCHOR_SEP}{content}").expect("write to String");
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LineEntry;
    use std::path::PathBuf;

    fn nth_anchor(n: usize) -> Anchor {
        let mut a = Anchor::first();
        for _ in 0..n {
            a.increment();
        }
        a
    }

    fn make(anchors: &[Anchor], lines: &[&str]) -> (FileAnchorState, Vec<String>) {
        let line_strs: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();
        let entries: Vec<LineEntry> = anchors
            .iter()
            .zip(&line_strs)
            .enumerate()
            .map(|(i, (a, _))| LineEntry {
                hash: i as u64,
                anchor: a.clone(),
            })
            .collect();
        let state = FileAnchorState {
            path: PathBuf::from("/x"),
            mtime_ns: 0,
            size: 0,
            content_digest: 0,
            lines: entries,
        };
        (state, line_strs)
    }

    #[test]
    fn file_render_format() {
        let anchors = [nth_anchor(0), nth_anchor(1)];
        let (state, lines) = make(&anchors, &["a", "b"]);
        let s = render_file(&state, &lines);
        let lines_out: Vec<&str> = s.lines().collect();
        assert_eq!(lines_out.len(), 2);
        for line in &lines_out {
            assert!(line.contains('§'));
        }
    }

    #[test]
    fn single_region_replace() {
        // pre: [A:foo, B:bar, C:baz]; post: [A:foo, M:qux, C:baz]
        let a = nth_anchor(0);
        let b = nth_anchor(1);
        let c = nth_anchor(2);
        let m = nth_anchor(10);
        let (pre_s, pre_l) = make(&[a.clone(), b.clone(), c.clone()], &["foo", "bar", "baz"]);
        let (post_s, post_l) = make(&[a, m, c], &["foo", "qux", "baz"]);

        let out = render_diff_block(&pre_s, &pre_l, &post_s, &post_l, 1);
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with(' '));
        assert!(lines[0].contains("§foo"));
        assert!(lines[1].starts_with('-'));
        assert!(lines[1].contains("§bar"));
        assert!(lines[2].starts_with('+'));
        assert!(lines[2].contains("§qux"));
        assert!(lines[3].starts_with(' '));
        assert!(lines[3].contains("§baz"));
    }

    #[test]
    fn pure_insertion_at_end() {
        let a = nth_anchor(0);
        let m = nth_anchor(5);
        let (pre_s, pre_l) = make(std::slice::from_ref(&a), &["foo"]);
        let (post_s, post_l) = make(&[a, m], &["foo", "bar"]);

        let out = render_diff_block(&pre_s, &pre_l, &post_s, &post_l, 1);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(' '));
        assert!(lines[0].contains("§foo"));
        assert!(lines[1].starts_with('+'));
        assert!(lines[1].contains("§bar"));
    }

    #[test]
    fn pure_deletion_at_start() {
        let a = nth_anchor(0);
        let b = nth_anchor(1);
        let (pre_s, pre_l) = make(&[a, b.clone()], &["foo", "bar"]);
        let (post_s, post_l) = make(&[b], &["bar"]);

        let out = render_diff_block(&pre_s, &pre_l, &post_s, &post_l, 1);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('-'));
        assert!(lines[0].contains("§foo"));
        assert!(lines[1].starts_with(' '));
        assert!(lines[1].contains("§bar"));
    }

    #[test]
    fn multi_region_separated_by_blank() {
        // pre:  A B C D E F G
        // post: A X C D E Y G  (B→X, F→Y; C/D/E unchanged in middle)
        let anchors_pre: Vec<Anchor> = (0..7).map(nth_anchor).collect();
        let mut anchors_post = anchors_pre.clone();
        anchors_post[1] = nth_anchor(20); // X
        anchors_post[5] = nth_anchor(21); // Y
        let (pre_s, pre_l) = make(
            &anchors_pre,
            &[
                "line0", "line1", "line2", "line3", "line4", "line5", "line6",
            ],
        );
        let (post_s, post_l) = make(
            &anchors_post,
            &["line0", "X1", "line2", "line3", "line4", "Y5", "line6"],
        );

        let out = render_diff_block(&pre_s, &pre_l, &post_s, &post_l, 1);
        // With context=1, the long Equal middle (line2/line3/line4 = 3 lines)
        // gets split: emit first 1 line, blank separator, last 1 line.
        // Should contain a blank line in the middle.
        assert!(out.contains("\n\n"));
        // Should contain both - and + sigils for both regions.
        assert_eq!(out.matches('-').count(), 2);
        assert_eq!(out.matches('+').count(), 2);
    }
}
