Read a previously-stored variable, optionally through a jq lens.

Args: `{ name, filter? }`

  name:   the variable name to look up.
  filter: optional jq filter. The stored value is bound as `.` and the filter's single output is returned. Must produce exactly one output (wrap with `[...]` to collect multiple into an array).

Without `filter`, returns the whole stored JSON value rendered as text (objects/arrays pretty-printed with two-space indent). With `filter`, returns the filter's result rendered the same way. Errors if no variable with that name has been set, or if the filter fails to compile/run.

The variable's value is `.` — NOT `$name`. Unlike `var_edit`, there are no `$name` bindings here. If I need to combine multiple variables in one filter, I use `var_edit` (which writes a destination).

WORKED EXAMPLES.

Pick one key out of a stored object:

  { "name": "plan", "filter": ".steps" }

Slice a stored array:

  { "name": "events", "filter": ".[10:20]" }

Slice two line ranges out of a multi-line string with line numbers (the affordance to reach for instead of shelling out to sed/python):

  { "name": "UI_RAW",
    "filter": "split(\"\n\") | to_entries | (.[459:640] + .[739:930]) | map(\"\(.key+1): \(.value)\") | join(\"\n\")" }
