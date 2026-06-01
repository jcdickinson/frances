# Edit tool friction log

Session: cursor-visibility + shift-enter fixes, 2025-06-27

## 1. Duplicate-anchor word collisions on single-line replaces

When the file contains multiple lines with identical *content*, the
anchor-word disambiguator sometimes picks the wrong instance **or**
refuses the edit outright with a trimmed-content-mismatch error.

**Example** — file has two `#[test]` lines (one orphaned above a
comment, one above a real function).  Replacing from the first
`#[test]` to a later anchor fails:

```
anchor 'Third§    #[test]' content mismatch (trimmed):
  file has     #[test],
  edit specified     #[test]
European§    // Cursor visibility…
```

The engine sees the *content* `#[test]` on the **second** occurrence
(decided by word lookup) and reports a mismatch because the surrounding
lines don't line up.

**Workaround:** expand the anchor range to include enough unique
surrounding context so the word resolves unambiguously, or use
`file_overwrite` / a shell `sed` for the tricky region.

**Suggested fix:** when the anchor word is *ambiguous* (appears on
multiple lines with the same trimmed content), report which occurrence
was tried and what the alternatives are, or accept a 1-indexed
occurrence hint in the call.

---

## 2. Multi-line anchor matching is brittle

`file_replace_lines` with `anchor`/`end_anchor` spanning several lines
is extremely sensitive to invisible whitespace or ordering differences.
A single trailing space or a blank line that the tool stripped during
rendering will cause a mismatch.

**Workaround:** single-line anchors are much more reliable. Prefer
anchoring on the first and last line of the range individually rather
than pasting a multi-line block as the anchor.

---

## 3. Inserting "after" a line that has an identical neighbor can target the wrong line

`file_insert_after` resolves the anchor to a *single* line, but when
two consecutive lines share the same content (and the anchor word
happens to collide), the insertion can land on the wrong side of the
pair.

**Workaround:** anchor on a unique nearby line and use
`file_insert_before` / `file_insert_after` with enough distance.

---

## 4. Comment-only block between two `#[test]` attributes created a stray `#[test]`

Replacing a test function with a block comment left behind the original
`#[test]` attribute line. The edit replaced only the *function body*
range, not the attribute. This produced a `#[test]` on a comment line,
which rustc flags as `duplicated attribute`.

**Workaround:** always include the `#[test]` attribute *inside* the
range being replaced/removed — don't assume the edit boundary will
clean it up.

---

## 5. Large diffs from small edits in untouched regions

Several `file_replace_lines` calls produced spurious diffs showing
lines far from the edit being reordered or re-anchored. The edits were
correct but the diff output was misleading and made it hard to verify
the change. This may be a display/cosmetic issue in the diff renderer
rather than a correctness bug.

---

## Summary of desired improvements

| Issue | Priority | Quick fix? |
|---|---|---|
| Ambiguous anchor words → mismatch error | High | Add occurrence index parameter |
| Multi-line anchor brittleness | Medium | Auto-trim & normalize whitespace |
| Stray `#[test]` after replacement | Low (user error) | Document the pitfall |
| Noisy diff on unrelated lines | Low | Investigate anchor reassignment |

