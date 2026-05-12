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

import { transcript, MarkdownFrame } from "frances:v1/frames";

const { Shell } = globalThis.__frances_v1_stash__;

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

class Run {
  static schema = RUN_SCHEMA;

  constructor(shell, { wait, kill, maxScolds = 2 } = {}) {
    this.shell = shell;
    this.wait = wait;
    this.kill = kill;
    // Number of "no forward progress" rounds we tolerate before giving
    // up and killing the in-flight command. The first round inside the
    // lock turn is free; after that, each non-progress round costs one
    // scold. When the budget hits zero, we SIGKILL the command.
    this.maxScolds = maxScolds;
    this.name = "shell_run";
    this.description =
      "Run a bash command. State (cwd, env, functions) persists across calls. " +
      "If output goes quiet before the command finishes, the result will say so — " +
      "call shell_wait to keep waiting or shell_kill to stop.";
    this.parameters = RUN_SCHEMA;
  }

  handler = async ({ call, scope }) => {
    let outcome;
    try {
      outcome = await this.shell.runOnce(call.arguments.cmd);
    } catch (err) {
      return _errResult(call.id, err);
    }
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
        const out = new MarkdownFrame({ content: "" });
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
          try {
            await shell.kill();
          } catch (_) {
            // Already settled — fine.
          }
          try {
            await shell.keepWaiting();
          } catch (_) {
            // Already idle — fine.
          }
          const killMsg =
            `Killed the shell command from ${call.id} — model did not call ` +
            `${waitName} or ${killName} after ${maxScolds} scold(s).`;
          transcript.push(new MarkdownFrame({ content: killMsg }));
          scope.push({ role: "user", content: killMsg });
          break;
        }
        scoldsRemaining -= 1;
        const scoldMsg =
          `Shell from ${call.id} is still running. ` +
          `You MUST call ${waitName} or ${killName} now.`;
        transcript.push(new MarkdownFrame({ content: scoldMsg }));
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
  }

  handler = async ({ call }) => {
    try {
      const outcome = await this.shell.keepWaiting();
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
      await this.shell.kill();
      // After kill, drain so the shell returns to idle and the model
      // sees the final exit status.
      let final;
      try {
        final = await this.shell.keepWaiting();
      } catch (_) {
        // Nothing in flight — already idle, no drain needed.
      }
      if (final) return _format(call.id, final);
      return {
        role: "tool",
        call_id: call.id,
        content: "killed (no in-flight command).",
        is_error: false,
      };
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

export { Shell, Run, Wait, Kill };
