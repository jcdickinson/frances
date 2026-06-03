Replace one contiguous range of lines in a file with new content.

Args: `{ path, anchor, end_anchor, text }`

  path:        the file to edit.
  anchor:      full rendered anchor line of the FIRST line in the range — `Word§content`, exactly as `file_read` produced it. Exactly ONE rendered line.
  end_anchor:  full rendered anchor line of the LAST line in the range (inclusive). For a single-line replace, pass the same value as `anchor`. Exactly ONE rendered line.

WORKED EXAMPLE. After `file_read` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Cherry§
  Daisy§def goodbye():

To replace the print with two prints:

{
  "path": "src/greet.py",
  "anchor":     "Banana§    print(\"hi\")",
  "end_anchor": "Banana§    print(\"hi\")",
  "text":       "    print(\"hi there\")\n    print(\"welcome\")"
}

Returns the diff block for the file with new anchors for inserted lines.

