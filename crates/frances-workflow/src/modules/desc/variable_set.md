Store a JSON value under a name. Overwrites any prior value with the same name.

Args: `{ name, value }`

  name:  the variable name to bind.
  value: any JSON value — object, array, string, number, boolean, or null.

Use this to remember structured state across turns: scratch notes, intermediate results, plans you intend to come back to. Other tools in the workflow can read the same variables, so this is also how you hand structured data to them.

The response reports only the stored type (e.g. `x = object(3 keys)`, `x = array(5 items)`, `x = string`) — NOT the value. If you see `string` when you expected an object or array, your tool-call double-encoded the JSON; either re-set with the value as a real JSON literal, or use `variable_assign` with `filter: "fromjson"` to unwrap it in place.
