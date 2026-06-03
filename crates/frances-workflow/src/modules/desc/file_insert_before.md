Insert new content immediately before a specific line in a file.

Args: `{ path, anchor, text }`

  path:    the file to edit.
  anchor:  full rendered anchor line that the new content will be inserted BEFORE — `Word§content`, exactly as `file_read` produced it. Exactly ONE rendered line.

WORKED EXAMPLE. After `file_read` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To add a comment before `goodbye`:

{
  "path": "src/greet.py",
  "anchor": "Daisy§def goodbye():",
  "text":   "# Says goodbye."
}

Returns the diff block for the file with new anchors for inserted lines.

