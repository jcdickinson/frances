// `frances:v1/tools/variable` — in-memory JSON variable store + per-op
// tool classes for `chat.tools.push(...)`.
//
// `Variables` is a plain JS holder over a `Map<string, JsonValue>`.
// Storage lives entirely in JS — there's no Rust backing — so values
// persist for the lifetime of the workflow invocation and no further.
//
// Other JS code (workflows, custom tools) can import `Variables` and
// call `.get(name)` / `.set(name, value)` directly. The `Get` / `Set` /
// `Assign` classes are thin LLM-facing wrappers that shape those
// methods into the `variable_get` / `variable_set` / `variable_assign`
// tool surface.
//
// `Assign` evaluates a jq filter through a Rust-side jaq bridge
// (`_jaqEval` on the install stash) — the filter's `.` is the
// destination's current value (or null) and `$name` bindings expose
// other stored variables. This gives the LLM a single tool for
// constructing, mutating, and introspecting JSON values.
//
// Typical wiring:
//
//   const vars = new Variables();
//   chat.tools.push(new Get(vars), new Set(vars), new Assign(vars));

const { VariableDescriptions: desc, _jaqEval } = globalThis.__frances_v1_stash__;

// ---- schemas --------------------------------------------------------------

const GET_SCHEMA = {
  type: "object",
  properties: {
    name: { type: "string" },
    filter: {
      type: "string",
      description:
        "Optional jq filter. The stored value is bound as `.`; the filter's single output is returned instead of the whole value. No `$name` bindings — use variable_assign to combine variables.",
    },
  },
  required: ["name"],
};

const SET_SCHEMA = {
  type: "object",
  properties: {
    name: { type: "string" },
    value: {
      description:
        "JSON value to store (object, array, string, number, boolean, or null).",
    },
  },
  required: ["name", "value"],
};

const ASSIGN_SCHEMA = {
  type: "object",
  properties: {
    name: { type: "string" },
    filter: { type: "string" },
    inputs: {
      type: "array",
      items: { type: "string" },
      description:
        "Optional names of other variables to expose inside the filter as $name bindings.",
    },
  },
  required: ["name", "filter"],
};

// ---- helpers --------------------------------------------------------------

function _okResult(call_id, content) {
  return { role: "tool", call_id, content, is_error: false };
}

function _errResult(call_id, err) {
  return {
    role: "tool",
    call_id,
    content: String((err && err.message) || err),
    is_error: true,
  };
}

// Compact human/LLM-readable shape summary. The signal we want to
// surface is the *type* (so e.g. qwen, which double-encodes array
// tool-call args as JSON strings, can spot that its array landed as
// `string` and call fromjson on the next read). For containers we
// also report the element count, which is free info and tells the
// model whether its write took the shape it expected.
function _describe(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return `array(${value.length} items)`;
  if (typeof value === "object") {
    return `object(${Object.keys(value).length} keys)`;
  }
  return typeof value;
}

// ---- storage --------------------------------------------------------------

class Variables {
  constructor() {
    this._store = new Map();
  }

  get(name) {
    return this._store.get(name);
  }

  set(name, value) {
    this._store.set(name, value);
  }

  has(name) {
    return this._store.has(name);
  }
}

// ---- tool classes ---------------------------------------------------------

class Get {
  static schema = GET_SCHEMA;

  constructor(vars) {
    this.vars = vars;
    this.name = "variable_get";
    this.description = desc.variable_get;
    this.parameters = GET_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.name) || "";
  }

  handler = async ({ call }) => {
    const { name, filter } = call.arguments;
    if (!this.vars.has(name)) {
      return _errResult(call.id, `unknown variable: ${name}`);
    }
    const value = this.vars.get(name);
    if (filter === undefined || filter === null) {
      return _okResult(call.id, JSON.stringify(value, null, 2));
    }
    let resultJson;
    try {
      resultJson = _jaqEval(filter, JSON.stringify(value), "{}");
    } catch (err) {
      return _errResult(call.id, err);
    }
    const filtered = JSON.parse(resultJson);
    return _okResult(call.id, JSON.stringify(filtered, null, 2));
  };
}

class Set {
  static schema = SET_SCHEMA;

  constructor(vars) {
    this.vars = vars;
    this.name = "variable_set";
    this.description = desc.variable_set;
    this.parameters = SET_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.name) || "";
  }

  handler = async ({ call }) => {
    const { name, value } = call.arguments;
    this.vars.set(name, value);
    return _okResult(call.id, `${name} = ${_describe(value)}`);
  };
}

class Assign {
  static schema = ASSIGN_SCHEMA;

  constructor(vars) {
    this.vars = vars;
    this.name = "variable_assign";
    this.description = desc.variable_assign;
    this.parameters = ASSIGN_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.name) || "";
  }

  handler = async ({ call }) => {
    const { name, filter, inputs } = call.arguments;
    const bindings = {};
    if (inputs) {
      for (const inputName of inputs) {
        if (!this.vars.has(inputName)) {
          return _errResult(call.id, `unknown variable: ${inputName}`);
        }
        bindings[inputName] = this.vars.get(inputName);
      }
    }
    const inputValue = this.vars.has(name) ? this.vars.get(name) : null;
    let resultJson;
    try {
      resultJson = _jaqEval(
        filter,
        JSON.stringify(inputValue),
        JSON.stringify(bindings),
      );
    } catch (err) {
      return _errResult(call.id, err);
    }
    const newValue = JSON.parse(resultJson);
    this.vars.set(name, newValue);
    return _okResult(call.id, `${name} = ${_describe(newValue)}`);
  };
}

export { Variables, Get, Set, Assign };
