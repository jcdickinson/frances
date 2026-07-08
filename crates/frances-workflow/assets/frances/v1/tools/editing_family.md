## Editing tools — shared protocol

Files must be read via `file_read` (without `into`) before editing.
`file_new` creates new files (no prior read needed); all other editing
tools require a prior read. `file_new` and `file_overwrite` echo back
fresh anchors for subsequent edits.

CRITICAL: `text` must NEVER contain a `Word§` prefix unless I
genuinely want those literal characters in the file (editing the anchor
engine itself, a test fixture, prose with `Word§`). Anchors are
read-only metadata the engine assigns — they're not part of line
content. If I paste back the rendered prefixes from a `file_read`,
they get written verbatim and my edit is broken.

  WRONG → text: "Apple§def hello():\nBanana§    print(\"hi\")"
  RIGHT → text: "def hello():\n    print(\"hi\")"

All write tools accept `text` or `from` (exactly one):
  text:  the raw content and NOTHING ELSE. Use `\n` for newlines.
         Multi-line is fine.
  from:  a Frances variable name. Its value is used as the content
         (string values pass through verbatim; non-string values are
         JSON-encoded). I use this when the content was prepared via
         `variable_set` / `variable_assign` / `file_read into:` /
         `shell_capture`, to avoid re-emitting a long payload in a
         tool-call.

Anchor protocol: every line in a `file_read` (or post-edit diff) is
rendered as `Word§content` — a stable per-line anchor word, then `§`,
then the line's content. The rendered string of each line is exactly
what I pass back as the `anchor` (and `end_anchor`) field of an
edit call. `anchor` and `end_anchor` are each a single rendered line
— never glue several `Word§content` lines into one field. A newline
in an anchor field is rejected. On a content mismatch the call fails
and I should re-read the file before retrying.

After every edit the file is run through the project formatter and
written to disk; the returned diff block reflects the post-format
content.
