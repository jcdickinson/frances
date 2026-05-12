// Frances primary workflow: an agentic chat with shell and file tools.
// User input becomes a user message; the LLM responds and can call
// shell_run/wait/kill against one long-lived bash subprocess, plus the
// anchor-aware file_read/replace/insert_*/new/overwrite family.
//
// Wire up via config:
//   [workflows.main]
//   id = "<uuid>"
//   file = "assets/workflows/main.ts"
//
// Type `quit` to exit.

import { inbox } from "frances:v1/inbox";
import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
import { ChatSession } from "frances:v1/chat";
import { Shell, Run, Wait, Kill } from "frances:v1/tools/shell";
import {
  Editor,
  Read,
  Replace,
  InsertAfter,
  InsertBefore,
  New,
  Overwrite,
} from "frances:v1/tools/file";
import { exit } from "frances:v1/workflow";

const chat = new ChatSession({ model_intents: ["chat"] });
chat.push({
  role: "system",
  content:
    "You are an agentic coding assistant. Use shell_run to execute bash. " +
    "If a command's output goes quiet before it finishes, decide whether to " +
    "keep waiting (shell_wait) or stop it (shell_kill). Shell state (cwd, env, " +
    "functions) persists across commands. " +
    "Use the file_* tools to read and edit files. Files are rendered with " +
    "stable per-line anchors (`Word§content`); pass an anchor line verbatim " +
    "back to the edit tools to identify the line you mean. Always file_read " +
    "before editing.",
});

const sh = new Shell();
const wait = new Wait(sh);
const kill = new Kill(sh);
const editor = new Editor();
chat.tools.push(
  new Run(sh, { wait, kill }),
  wait,
  kill,
  new Read(editor),
  new Replace(editor),
  new InsertAfter(editor),
  new InsertBefore(editor),
  new New(editor),
  new Overwrite(editor),
);

transcript.push(
  new MarkdownFrame({
    content: "frances ready. Type `quit` to exit.",
  }),
);

try {
  for await (const input of inbox) {
    const msg = input.content.trim();
    if (msg === "quit") {
      transcript.push(new MarkdownFrame({ content: "bye" }));
      exit();
      break;
    }

    chat.push({ role: "user", content: msg });
    try {
      while (true) {
        const out = new MarkdownFrame({ content: "" });
        transcript.push(out);
        const r = await chat.stream();
        await r.text.pipeTo(out.writable);
        const { tool_calls } = await r.completed;
        if (!tool_calls || tool_calls.length === 0) break;
      }
    } catch (e) {
      transcript.push(new ErrorFrame({ content: `chat failed: \`${e}\`` }));
    }
  }
} finally {
  await sh.close();
}
