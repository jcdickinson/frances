// Sample workflow exercising the permanent host API.
// Wire up via config:
//   [workflows.main]
//   id = "<uuid>"
//   file = "assets/workflows/main.ts"
// Then run `/main`, send a few messages, and type "quit" to exit.

declare const workflow: {
  frame: {
    text(s: unknown): void;
    error(s: unknown): void;
    json(tag: string, value: unknown): void;
  };
  user: {
    input: AsyncIterableIterator<{ message: string }>;
  };
  exit(): void;
};

declare global {
  interface ImportMeta {
    args: string[];
  }
}

const args = import.meta.args;
workflow.frame.text(`/main started${args.length ? ` (${args.join(" ")})` : ""}`);
workflow.frame.text("type 'quit' to exit, anything else to echo");

let n = 0;
for await (const input of workflow.user.input) {
  n += 1;
  const msg = input.message.trim();
  if (msg === "quit") {
    workflow.frame.text(`bye after ${n} message${n === 1 ? "" : "s"}`);
    workflow.exit();
    break;
  }
  workflow.frame.json("echo", { n, message: input.message });
}
