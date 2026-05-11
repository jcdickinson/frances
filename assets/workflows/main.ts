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
import { Timer } from "frances:v1/io";
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
      "- `md` — push a MarkdownFrame and stream into it via `append` (250ms ticks)\n" +
      "- `err` — push an ErrorFrame\n" +
      "- `json` — push a JsonFrame\n" +
      "- `supersede` — show that `append` on a superseded frame throws\n" +
      "- `chat <prompt>` — run a chat turn against the shared session (history persists)\n" +
      "- `quit` — exit\n" +
      "- anything else — echo as JSON",
  }),
);

// One chat session for the lifetime of the workflow — history accumulates
// across `chat` invocations.
const chat = new ChatSession({ model_intents: ["chat"] });
chat.push({ role: "system", content: "You are a terse, friendly assistant." });

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
    const tick = new Timer({ interval: 250 });
    for (let i = 1; i <= 3; i += 1) {
      await tick;
      f.append(`\n  - step ${i}`);
    }
    tick.disable();
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

  if (msg.startsWith("chat")) {
    const prompt = msg.slice("chat".length).trim();
    if (!prompt) {
      transcript.push(
        new ErrorFrame({ content: "usage: chat <prompt>" }),
      );
      continue;
    }
    chat.push({ role: "user", content: prompt });
    const r = await chat.stream();
    const out = new MarkdownFrame({ content: "" });
    transcript.push(out);
    for await (const ev of r.events) {
      if (ev.type === "text") out.append(ev.delta);
    }
    try {
      const final = await r.completed;
      transcript.push(
        new JsonFrame({ tag: "chat-usage", value: final.usage ?? null }),
      );
    } catch (e) {
      transcript.push(
        new ErrorFrame({ content: `chat failed: \`${e}\`` }),
      );
    }
    continue;
  }

  transcript.push(
    new JsonFrame({ tag: "echo", value: { n, message: input.content } }),
  );
}
