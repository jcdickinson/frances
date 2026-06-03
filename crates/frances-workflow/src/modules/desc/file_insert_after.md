Insert new content immediately after a specific line in a file.

Args: `{ path, anchor, text }`

  path:    the file to edit.
  anchor:  full rendered anchor line that the new content will be inserted AFTER — `Word§content`, exactly as `file_read` produced it. Exactly ONE rendered line.

WORKED EXAMPLE. After `file_read` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To insert a docstring before `goodbye` (which is the same as inserting AFTER the blank line — but if there isn't one, you can pin to `Banana` and insert after that, or use `file_insert_before` on `Daisy`). To add a comment after the print:

{
  "path": "src/greet.py",
  "anchor": "Banana§    print(\"hi\")",
  "text":   "    # printed greeting"
}

Returns the diff block for the file with new anchors for inserted lines.

