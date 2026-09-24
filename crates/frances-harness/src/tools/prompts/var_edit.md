Change a stored variable by running a jq filter over it. This is the tool for a value derived from what I already have; to store a literal value I am holding, I use `var_set` and skip the jq escaping.

The destination's prior value is the `.` input to the filter (or `null` if it isn't set yet). Variables listed in `inputs` are bound as `$name` inside the filter.

Args: `{ name, filter, inputs? }`

  name:    destination variable. Read in as `.` and overwritten by the filter's result.
  filter:  a jq filter expression. Must produce exactly one output — wrap with `[...]` if I want an array.
  inputs:  optional array of variable names to expose inside the filter as `$name` bindings. Each must already be set.

I use this to:

- mutate a stored value (`'. + 1'`, `'.done = true'`, `'.steps += ["c"]'`).
- introspect with `keys`, `keys_unsorted`, `length`, `type`, `to_entries`, `has(...)`, `paths`, `leaf_paths`.
- parse / serialise text with `fromjson` / `tojson`.
- combine variables: `{ name: "merged", filter: "$a * $b", inputs: ["a","b"] }` deep-merges two stored objects into a new one.

WORKED EXAMPLES.

Increment:

  { "name": "counter", "filter": ". + 1" }

Append to nested array:

  { "name": "plan", "filter": ".steps += [\"c\"]" }

List the keys of another variable:

  { "name": "plan_keys", "filter": "$plan | keys", "inputs": ["plan"] }

The response reports only the stored type (e.g. `plan = object(2 keys)`, `obj_keys = array(2 items)`, `counter = number`) — NOT the value. I will call `var_get` if I need to see it. If the type isn't what I expected (e.g. `string` when I wrote a filter that should yield an object), the filter or the input shape is wrong.
