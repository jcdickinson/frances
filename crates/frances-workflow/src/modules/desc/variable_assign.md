Evaluate a jq filter and store the result into a variable. The destination's prior value is the `.` input to the filter (or `null` if it isn't set yet). Variables listed in `inputs` are bound as `$name` inside the filter.

Args: `{ name, filter, inputs? }`

  name:    destination variable. Read in as `.` and overwritten by the filter's result.
  filter:  a jq filter expression. Must produce exactly one output — wrap with `[...]` if I want an array.
  inputs:  optional array of variable names to expose inside the filter as `$name` bindings. Each must already be set.

I use this to:

- create / overwrite values, including constructing objects and arrays from scratch (e.g. `'{"steps": ["a","b"], "done": false}'`).
- mutate a stored value (`'. + 1'`, `'.done = true'`, `'.steps += ["c"]'`).
- introspect with `keys`, `keys_unsorted`, `length`, `type`, `to_entries`, `has(...)`, `paths`, `leaf_paths`.
- parse / serialise text with `fromjson` / `tojson`.
- combine variables: `assign({ name: "merged", filter: "$a * $b", inputs: ["a","b"] })` deep-merges two stored objects.

WORKED EXAMPLES.

Fresh value:

  { "name": "plan", "filter": "{steps: [\"a\",\"b\"], done: false}" }

Increment:

  { "name": "counter", "filter": ". + 1" }

Append to nested array:

  { "name": "plan", "filter": ".steps += [\"c\"]" }

List the keys of another variable:

  { "name": "plan_keys", "filter": "$plan | keys", "inputs": ["plan"] }

The response reports only the stored type (e.g. `plan = object(2 keys)`, `obj_keys = array(2 items)`, `counter = number`) — NOT the value. I will call `variable_get` if I need to see it. If the type isn't what I expected (e.g. `string` when I wrote a filter that should yield an object), the filter or the input shape is wrong.
