Store a JSON value under a name. Overwrites any prior value with the same name.

Args: `{ name, value }`

  name:  the variable name to bind.
  value: any JSON value — object, array, string, number, boolean, or null.

I use this to remember structured state across turns: scratch notes, intermediate results, plans I intend to come back to. Other tools in the workflow can read the same variables, so this is also how I hand structured data to them.

The response reports only the stored type (e.g. `x = object(3 keys)`, `x = array(5 items)`, `x = string`) — NOT the value. If I see `string` when I expected an object or array, my tool-call double-encoded the JSON; I either re-set with the value as a real JSON literal, or use `variable_assign` with `filter: "fromjson"` to unwrap it in place.
