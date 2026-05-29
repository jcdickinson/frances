// `frances:v1/tools/shell` — bash primitive + Run/Wait/Kill tool
// classes for `chat.tools.push(...)`.
//
// The `Shell` class (Rust-backed) owns one bash subprocess. The
// `Run`/`Wait`/`Kill` classes are thin JS wrappers around it, shaped
// for the LLM tool API: each exposes `name`, `description`,
// `parameters` (with a static `.schema` so workflows can compose their
// own variants) and a `handler({call, scope})`.
//
// `Run` is constructed with references to its companion `Wait`/`Kill`:
//
//   const sh = new Shell();
//   const wait = new Wait(sh);
//   const kill = new Kill(sh);
//   chat.tools.push(new Run(sh, { wait, kill }), wait, kill);
//
// By default `Run.handler` asks the user to approve each command via
// `frances:v1/approval` before executing it. Pass `{ approve: false }`
// to bypass the gate (e.g. for tests or trusted workflows).
//
// When `Run` returns a `Done` or `Dead` outcome it just returns a
// tool_result like a leaf. When the command goes `Quiet`, `Run`:
//   1. returns a "still running" tool_result for the initial batch, and
//   2. registers a post-batch turn via `scope.lock`. The turn installs a
//      `scope.toolCall` hook that lets only the companion `Wait`/`Kill`
//      tools through (anything else gets scolded with an error result),
//      then loops `scope.stream` until the shell is no longer running.
//
// During the lock turn, the LLM's text and the scold / kill notices get
// rendered to the transcript so the user can see what's happening (the
// inner rounds are inside the outer scope.stream's resolution, so the
// workflow's outer pipeTo doesn't see them). The scold / kill text is
// also pushed to chat history so the LLM acts on it.
//
// The leaf `Wait` and `Kill` tools are unchanged from before.
//
// Two more leaf tools — `Set` and `Capture` — bridge Frances variables
// to/from bash variables. `Set` writes a Frances var into either a
// shell variable (`set:`) or an exported env var (`export:`); `Capture`
// reads a bash variable back into a Frances var. They run short
// deterministic bash commands through the same `Shell` and share its
// busy/closed state. Both take a `Variables` instance:
//
//   const vars = new Variables();
//   chat.tools.push(new Set(sh, vars), new Capture(sh, vars));

import {
  transcript,
  MarkdownSection,
  ShellOutputSection,
} from "frances:v1/sections";
import { approve } from "frances:v1/approval";

const { Shell, ShellDescriptions: shellDesc } = globalThis.__frances_v1_stash__;

const RUN_SCHEMA = {
  type: "object",
  properties: {
    cmd: {
      type: "string",
      description:
        "Bash code to run. Multi-line, pipelines, heredocs, etc. all work.",
    },
  },
  required: ["cmd"],
};

const WAIT_SCHEMA = {
  type: "object",
  properties: {},
};

const KILL_SCHEMA = {
  type: "object",
  properties: {},
};

function _format(call_id, outcome) {
  if (outcome.kind === "done") {
    return {
      role: "tool",
      call_id,
      content: `Exit ${outcome.exit_code}\n${outcome.output}`,
      is_error: outcome.exit_code !== 0,
    };
  }
  if (outcome.kind === "quiet") {
    return {
      role: "tool",
      call_id,
      content:
        `Still running (${outcome.reason}). Output so far:\n${outcome.output}\n` +
        `Call shell_wait to keep waiting, or shell_kill to stop.`,
      is_error: false,
    };
  }
  // dead
  return {
    role: "tool",
    call_id,
    content: `Bash exited. Output:\n${outcome.output}`,
    is_error: true,
  };
}

function _errResult(call_id, err) {
  return {
    role: "tool",
    call_id,
    content: String((err && err.message) || err),
    is_error: true,
  };
}

// Run `op` (a `runOnce`/`keepWaiting` invocation) while concurrently
// pulling discrete `ReadEvent`s off the shell's event stream. Output
// events go straight to the open frame's writer as they arrive; the
// terminal event (`done` / `quiet` / `dead`) ends the loop and the
// matching outcome is resolved by `op`'s promise.
async function _streamUntilSettled(shell, op) {
  const opPromise = op();
  while (true) {
    const event = await shell.nextEvent();
    if (event === null) {
      // All senders gone — shell was closed mid-call. Fall through
      // and let `op` resolve with whatever error/outcome Rust returns.
      break;
    }
    if (event.kind === "output") {
      const writer = shell._writer;
      if (writer) {
        try {
          await writer.write(event.data);
        } catch (_) {
          // Writer closed/errored — drop silently.
        }
      }
      continue;
    }
    // Terminal: "done" / "quiet" / "dead". `op` will resolve with the
    // matching `Outcome` immediately after.
    break;
  }
  return await opPromise;
}

// The visible frame for the currently-running shell command — Run
// pushes it, Wait/Kill append to it. A Shell instance only runs one
// command at a time so a single slot is enough.
//
// `frame` is a `ShellOutputSection`; `writer` is its writable's locked
// writer (acquired once per frame so we don't fight reacquisition).
function _openShellFrame(shell, cmd) {
  // Defensive: a previous frame should have been finalized by Done/
  // Dead/kill. If one's still around, close it before opening a new
  // one so the scrollback row gets persisted.
  if (shell._frame) {
    _closeShellFrame(shell);
  }
  const frame = new ShellOutputSection({ cmd });
  transcript.push(frame);
  const writer = frame.writable.getWriter();
  shell._frame = frame;
  shell._writer = writer;
  return { frame, writer };
}

// Append `text` to the open shell frame. The pump owns bash output;
// this is for synthetic JS-side text (e.g. the "(killed)" tail).
// No-op if no frame is open.
async function _appendShellOutput(shell, text) {
  if (!shell._writer || !text) return;
  try {
    await shell._writer.write(text);
  } catch (_) {
    // Writer closed/errored — drop silently.
  }
}

// Set the frame's terminal state (`Success`/`Exit(N)`) and close the
// writable. Autoclose on the writable's sink fires `frame.close()`.
// No-op if there's no open frame.
async function _closeShellFrame(shell, terminal) {
  const frame = shell._frame;
  const writer = shell._writer;
  shell._frame = null;
  shell._writer = null;
  if (!frame) return;
  if (terminal === "success") {
    frame.success();
  } else if (terminal && typeof terminal.exit === "number") {
    frame.exit(terminal.exit);
  }
  if (writer) {
    try {
      await writer.close();
    } catch (_) {
      // Already closed/errored — fine.
    }
  } else {
    // No writer? Close the frame directly so the runtime gets a Close.
    try {
      frame.close();
    } catch (_) {
      // Already closed — fine.
    }
  }
}

// Finalise the frame for a `runOnce`/`keepWaiting` outcome. The pump
// has already streamed `outcome.output` into the frame as bytes
// arrived; this function only handles the per-outcome epilogue
// (terminal-state transition + close, optional synthetic "killed"
// tail). Quiet leaves the frame open. Returns the outcome unchanged.
async function _frameOutcome(shell, outcome, killedSuffix) {
  if (killedSuffix) {
    await _appendShellOutput(shell, killedSuffix);
  }
  if (outcome.kind === "done") {
    await _closeShellFrame(
      shell,
      outcome.exit_code === 0 ? "success" : { exit: outcome.exit_code },
    );
  } else if (outcome.kind === "dead") {
    // Bash itself exited. We don't know the cause; use -1 as a
    // sentinel so the TUI renders it red without colliding with a
    // typical exit code.
    await _closeShellFrame(shell, { exit: -1 });
  }
  // Quiet — leave the frame open.
  return outcome;
}

// Ask the user to approve a `shell_run` call. Returns `null` if the
// user said yes and the command should proceed, otherwise a fully
// formed tool_result the handler should return verbatim.
async function _askApproval(call) {
  const cmd = call.arguments.cmd;
  const prompt =
    "Allow Frances to run this bash command?\n\n" +
    "```bash\n" +
    cmd +
    "\n```";
  let choice;
  try {
    choice = await approve({
      prompt,
      toolCall: { id: call.id, name: call.name, arguments: call.arguments },
      // Opt this gate into the runtime's auto-judge. If `models.auto`
      // (with fallback `referee`, then `cheap`) is configured, the
      // runtime may approve without showing the user a prompt. On
      // judge reject / error the request falls through to the user
      // exactly as if `allowAuto` were false.
      allowAuto: true,
    });
  } catch (err) {
    return _errResult(call.id, err);
  }
  if (choice.type === "yes") return null;
  // `No` — either the user said no, or the runtime translated a
  // chat-redirect into a `No` (the user's text is dispatched as a
  // fresh prompt; we just return a denied tool_result here).
  const reason = choice.details ? ` Reason: ${choice.details}` : "";
  return {
    role: "tool",
    call_id: call.id,
    content: `User denied this shell command.${reason}`,
    is_error: true,
  };
}

class Run {
  static schema = RUN_SCHEMA;

  constructor(shell, { wait, kill, maxScolds = 2, approve: approveOpt = true } = {}) {
    this.shell = shell;
    this.wait = wait;
    this.kill = kill;
    // Number of "no forward progress" rounds we tolerate before giving
    // up and killing the in-flight command. The first round inside the
    // lock turn is free; after that, each non-progress round costs one
    // scold. When the budget hits zero, we SIGKILL the command.
    this.maxScolds = maxScolds;
    // Whether to gate each command behind `approve()`. Default on;
    // opt out with `{ approve: false }`.
    this.requireApproval = approveOpt;
    this.name = "shell_run";
    this.description =
      "Run a bash command. State (cwd, env, functions) persists across calls. " +
      "If output goes quiet before the command finishes, the result will say so — " +
      "call shell_wait to keep waiting or shell_kill to stop.";
    this.parameters = RUN_SCHEMA;
  }

  describe(call) {
    const cmd = call.arguments && call.arguments.cmd;
    return typeof cmd === "string" ? cmd : "";
  }

  handler = async ({ call, scope }) => {
    if (this.requireApproval) {
      const gate = await _askApproval(call);
      if (gate !== null) return gate;
    }

    _openShellFrame(this.shell, call.arguments.cmd);
    let outcome;
    try {
      outcome = await _streamUntilSettled(this.shell, () =>
        this.shell.runOnce(call.arguments.cmd),
      );
    } catch (err) {
      // The frame is open but the command never produced an outcome;
      // close it as exit(-1) so the user sees a terminal state.
      await _closeShellFrame(this.shell, { exit: -1 });
      return _errResult(call.id, err);
    }
    await _frameOutcome(this.shell, outcome);
    if (outcome.kind !== "quiet") return _format(call.id, outcome);

    // Quiet: register a post-batch turn to negotiate wait/kill with the
    // model. The initial tool_result for this call still goes out below
    // (so the batch's history stays valid).
    const waitName = this.wait ? this.wait.name : "shell_wait";
    const killName = this.kill ? this.kill.name : "shell_kill";
    const shell = this.shell;
    const maxScolds = this.maxScolds;
    scope.lock(async () => {
      scope.toolCall = async ({ call: c, invoke }) => {
        if (c.name !== waitName && c.name !== killName) {
          throw new Error(
            `'${c.name}' is disabled while resolving shell from ${call.id}; ` +
              `use ${waitName} or ${killName} instead`,
          );
        }
        return await invoke();
      };
      // Drive the negotiation. The model gets `maxScolds + 1` chances
      // total: the initial round (free) plus `maxScolds` scolded
      // re-prompts. After that we SIGKILL the in-flight command so the
      // shell can return to idle.
      let scoldsRemaining = maxScolds;
      while (await shell.isRunning()) {
        // Render the inner round's LLM text into a frame.
        const out = new MarkdownSection();
        transcript.push(out);
        const r = await scope.stream();
        await r.text.pipeTo(out.writable);
        const { tool_calls } = await r.completed;

        // Forward progress means the model called wait or kill. Off-
        // script tool calls (caught by scope.toolCall) and empty
        // responses count as no progress.
        const madeProgress =
          tool_calls &&
          tool_calls.some((c) => c.name === waitName || c.name === killName);
        if (madeProgress) {
          scoldsRemaining = maxScolds;
          continue;
        }

        if (scoldsRemaining <= 0) {
          // Budget exhausted — kill the in-flight command and drain.
          // Both phases' outputs land in the shell frame so the user
          // sees the final state.
          try {
            await shell.kill();
          } catch (_) {
            // Already settled — fine.
          }
          try {
            const drained = await _streamUntilSettled(shell, () =>
              shell.keepWaiting(),
            );
            await _frameOutcome(shell, drained, "\n(killed)");
          } catch (_) {
            // Already idle — close the frame defensively if anyone
            // left it open.
            await _closeShellFrame(shell, { exit: -1 });
          }
          const killMsg =
            `Killed the shell command from ${call.id} — model did not call ` +
            `${waitName} or ${killName} after ${maxScolds} scold(s).`;
          transcript.push(
            new MarkdownSection({ content: killMsg, closed: true }),
          );
          scope.push({ role: "user", content: killMsg });
          break;
        }
        scoldsRemaining -= 1;
        const scoldMsg =
          `Shell from ${call.id} is still running. ` +
          `You MUST call ${waitName} or ${killName} now.`;
        transcript.push(
          new MarkdownSection({ content: scoldMsg, closed: true }),
        );
        scope.push({ role: "user", content: scoldMsg });
      }
    });

    return _format(call.id, outcome);
  };
}

class Wait {
  static schema = WAIT_SCHEMA;

  constructor(shell) {
    this.shell = shell;
    this.name = "shell_wait";
    this.description =
      "Continue waiting on the in-flight shell command. Returns when it finishes or goes quiet again.";
    this.parameters = WAIT_SCHEMA;
    this.hidden = true;
  }

  handler = async ({ call }) => {
    try {
      const outcome = await _streamUntilSettled(this.shell, () =>
        this.shell.keepWaiting(),
      );
      await _frameOutcome(this.shell, outcome);
      return _format(call.id, outcome);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class Kill {
  static schema = KILL_SCHEMA;

  constructor(shell) {
    this.shell = shell;
    this.name = "shell_kill";
    this.description = "SIGKILL the in-flight shell command.";
    this.parameters = KILL_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      // Drain after SIGKILL. Loop a few attempts with re-kills between
      // them: in the common case bash flushes its "Killed" notice and
      // the wrapper's post-source sentinel well within one wait. We
      // retry because a single Quiet here is what used to drive the
      // model into a `shell_kill` loop (Quiet formats as "Still
      // running … call shell_kill to stop").
      let final = null;
      let drained = false;
      const MAX_ATTEMPTS = 3;
      for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt += 1) {
        // `shell.kill()` only throws when the shell is closed —
        // surface that to the model via the outer catch instead of
        // swallowing it.
        await this.shell.kill();
        if (!(await this.shell.isRunning())) {
          drained = true;
          break;
        }
        try {
          final = await _streamUntilSettled(this.shell, () =>
            this.shell.keepWaiting(),
          );
        } catch (_) {
          // Rust says no command in flight — already idle.
          final = null;
          drained = true;
          break;
        }
        if (final.kind !== "quiet") {
          drained = true;
          break;
        }
      }
      if (drained && final) {
        await _frameOutcome(this.shell, final, "\n(killed)");
        return _format(call.id, final);
      }
      if (drained) {
        // Nothing was in flight by the time we got here.
        await _closeShellFrame(this.shell, { exit: -1 });
        return {
          role: "tool",
          call_id: call.id,
          content: "killed (no in-flight command).",
          is_error: false,
        };
      }
      // Bash didn't return to idle after MAX_ATTEMPTS rounds of
      // SIGKILL + drain. Slam the door: close the shell so future
      // shell_run calls fail loudly instead of inheriting a wedged
      // bash, and tell the model directly so it doesn't fire another
      // shell_kill.
      await _appendShellOutput(this.shell, "\n(killed; shell closed)");
      await _closeShellFrame(this.shell, { exit: -1 });
      try {
        await this.shell.close();
      } catch (_) {
        // Already closed — fine.
      }
      return {
        role: "tool",
        call_id: call.id,
        content:
          "SIGKILL sent but bash did not return to idle after several attempts. Closed the shell. Further shell_run / shell_wait / shell_kill calls on it will error.",
        is_error: true,
      };
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

// ---- shell↔Frances variable bridges --------------------------------------

const SET_SCHEMA = {
  type: "object",
  properties: {
    set: {
      type: "string",
      description:
        "Bash variable name to assign as a plain shell variable. Visible only inside the current bash session; subprocesses do NOT inherit it. Provide exactly one of `set` or `export`.",
    },
    export: {
      type: "string",
      description:
        "Bash variable name to assign AND export. Same as `set` but also exported so subprocesses inherit the value. Provide exactly one of `set` or `export`.",
    },
    from: {
      type: "string",
      description:
        "Frances variable name to pull the value from. Strings pass through verbatim; non-strings are JSON-encoded.",
    },
  },
  required: ["from"],
};

const CAPTURE_SCHEMA = {
  type: "object",
  properties: {
    name: {
      type: "string",
      description:
        "Frances variable name to store the captured value into.",
    },
    from: {
      type: "string",
      description: "Bash variable name to read.",
    },
  },
  required: ["name", "from"],
};

function _stringify(value) {
  return typeof value === "string" ? value : JSON.stringify(value);
}

class Set {
  static schema = SET_SCHEMA;

  constructor(shell, vars) {
    this.shell = shell;
    this.vars = vars;
    this.name = "shell_set";
    this.description = shellDesc.shell_set;
    this.parameters = SET_SCHEMA;
  }

  describe(call) {
    const a = call.arguments || {};
    const target = a.export || a.set;
    if (!target) return "";
    const from = a.from ? ` ← ${a.from}` : "";
    return `$${target}${from}`;
  }

  handler = async ({ call }) => {
    const args = call.arguments;
    const hasSet = typeof args.set === "string" && args.set.length > 0;
    const hasExport = typeof args.export === "string" && args.export.length > 0;
    if (hasSet && hasExport) {
      return _errResult(
        call.id,
        "provide exactly one of `set` or `export`, not both",
      );
    }
    if (!hasSet && !hasExport) {
      return _errResult(call.id, "provide exactly one of `set` or `export`");
    }
    const from = args.from;
    if (typeof from !== "string" || from.length === 0) {
      return _errResult(call.id, "missing `from` (Frances variable name)");
    }
    if (!this.vars.has(from)) {
      return _errResult(call.id, `unknown variable: ${from}`);
    }
    const bashName = hasSet ? args.set : args.export;
    const exported = hasExport;
    try {
      await this.shell.setVar(bashName, _stringify(this.vars.get(from)), exported);
    } catch (err) {
      return _errResult(call.id, err);
    }
    const verb = exported ? "exported" : "set";
    return {
      role: "tool",
      call_id: call.id,
      content: `${verb} $${bashName} from ${from}`,
      is_error: false,
    };
  };
}

class Capture {
  static schema = CAPTURE_SCHEMA;

  constructor(shell, vars) {
    this.shell = shell;
    this.vars = vars;
    this.name = "shell_capture";
    this.description = shellDesc.shell_capture;
    this.parameters = CAPTURE_SCHEMA;
  }

  describe(call) {
    const a = call.arguments || {};
    if (!a.name || !a.from) return a.name || "";
    return `${a.name} ← $${a.from}`;
  }

  handler = async ({ call }) => {
    const { name, from } = call.arguments;
    let captured;
    try {
      captured = await this.shell.captureVar(from);
    } catch (err) {
      return _errResult(call.id, err);
    }
    this.vars.set(name, captured);
    return {
      role: "tool",
      call_id: call.id,
      content: `${name} = string`,
      is_error: false,
    };
  };
}

export { Shell, Run, Wait, Kill, Set, Capture };
