// Playground workflow exercising the full frances:v1/* import surface.
// Wire up via config:
//   [workflows.main]
//   id = "<uuid>"
//   file = "assets/workflows/main.ts"
//
// Then run `/main` and try the commands listed in the opening frame.

import { inbox } from "frances:v1/inbox";
import {
  transcript,
  MarkdownFrame,
  ErrorFrame,
  JsonFrame,
} from "frances:v1/frames";
import { ChatSession } from "frances:v1/chat";
import { exit } from "frances:v1/workflow";

declare global {
  interface ImportMeta {
    args: string[];
  }
}

const args = import.meta.args;
const argsLine = args.length ? ` (args: ${args.join(", ")})` : "";

transcript.push(
  new MarkdownFrame({
    content:
      `/main started${argsLine}\n\n` +
      "**Commands**\n" +
      "- `md` — push a MarkdownFrame and stream into it via `append`\n" +
      "- `err` — push an ErrorFrame\n" +
      "- `json` — push a JsonFrame\n" +
      "- `supersede` — show that `append` on a superseded frame throws\n" +
      "- `chat` — exercise ChatSession (push system + user, then `stream` throws)\n" +
      "- `assistant` — show that `role: 'assistant'` is rejected\n" +
      "- `quit` — exit\n" +
      "- anything else — echo as JSON",
  }),
);

let n = 0;

for await (const input of inbox) {
  n += 1;
  const msg = input.content.trim();

  if (msg === "quit") {
    transcript.push(
      new MarkdownFrame({
        content: `bye after ${n} message${n === 1 ? "" : "s"}`,
      }),
    );
    exit();
    break;
  }

  if (msg === "md") {
    const f = new MarkdownFrame({ content: "streaming markdown:" });
    transcript.push(f);
    for (let i = 1; i <= 3; i += 1) {
      f.append(`\n  - step ${i}`);
    }
    continue;
  }

  if (msg === "err") {
    transcript.push(
      new ErrorFrame({ content: "this is what an ErrorFrame looks like" }),
    );
    continue;
  }

  if (msg === "json") {
    transcript.push(
      new JsonFrame({
        tag: "demo",
        value: { hello: "world", count: 42 },
      }),
    );
    continue;
  }

  if (msg === "supersede") {
    const a = new MarkdownFrame({ content: "frame A (active until next push)" });
    transcript.push(a);
    transcript.push(new MarkdownFrame({ content: "frame B (now active)" }));
    try {
      a.append(" tried to append to A");
      transcript.push(
        new ErrorFrame({ content: "BUG: append on superseded frame did not throw" }),
      );
    } catch (e) {
      transcript.push(
        new MarkdownFrame({ content: `append on superseded frame threw (expected): \`${e}\`` }),
      );
    }
    continue;
  }

  if (msg === "chat") {
    const s = new ChatSession({ model_intents: ["chat"] });
    s.push({ role: "system", content: "you are a summariser" });
    s.push({ role: "user", content: "hello" });
    try {
      s.push({ role: "system", content: "too late" });
      transcript.push(
        new ErrorFrame({ content: "BUG: system-after-user did not throw" }),
      );
    } catch (e) {
      transcript.push(
        new MarkdownFrame({ content: `system-after-user threw (expected): \`${e}\`` }),
      );
    }
    try {
      s.stream();
      transcript.push(
        new ErrorFrame({ content: "BUG: stream did not throw" }),
      );
    } catch (e) {
      transcript.push(
        new MarkdownFrame({ content: `stream threw (expected, backend pending): \`${e}\`` }),
      );
    }
    continue;
  }

  if (msg === "assistant") {
    const s = new ChatSession({ model_intents: ["x"] });
    try {
      s.push({ role: "assistant", content: "no" });
      transcript.push(
        new ErrorFrame({ content: "BUG: push assistant did not throw" }),
      );
    } catch (e) {
      transcript.push(
        new MarkdownFrame({ content: `push assistant threw (expected): \`${e}\`` }),
      );
    }
    continue;
  }

  transcript.push(
    new JsonFrame({ tag: "echo", value: { n, message: input.content } }),
  );
}
