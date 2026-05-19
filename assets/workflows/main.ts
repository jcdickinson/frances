// Frances primary workflow: an agentic chat with shell and file tools.
// User input becomes a user message; the LLM responds and can call
// shell_run/wait/kill against one long-lived bash subprocess, plus the
// anchor-aware file_read/replace/insert_*/new/overwrite family.
//
// This version also keeps a rudimentary in-memory agentic loop:
//   - a living typed plan (`plan_update`)
//   - a structured task-completion signal (`task_complete`)
//   - a cheap/referee model that approves or declines completion
//   - context reset after each approved task, seeded with the plan state
//
// Wire up as the daemon's default workflow via config:
//   default_workflow = "main"
//
//   [workflows.main]
//   id = "<uuid>"
//   file = "assets/workflows/main.ts"
//
// Type `quit` to exit.

import { inbox } from "frances:v1/inbox";
import { transcript, MarkdownFrame, ErrorFrame } from "frances:v1/frames";
import { ChatSession } from "frances:v1/chat";
import {
  Shell,
  Run,
  Wait,
  Kill,
  Set as ShellSet,
  Capture as ShellCapture,
} from "frances:v1/tools/shell";
import {
  Editor,
  Read,
  Replace,
  InsertAfter,
  InsertBefore,
  New,
  Overwrite,
} from "frances:v1/tools/file";
import { FileSearch, Search } from "frances:v1/tools/file_find_or_grep";
import {
  Variables,
  Get as VarGet,
  Set as VarSet,
  Assign as VarAssign,
} from "frances:v1/tools/variable";
import { exit } from "frances:v1/workflow";

type StepStatus = "pending" | "active" | "completed" | "abandoned";
type StepOutcome = "succeeded" | "partial" | "failed" | "abandoned";

type PlanStep = {
  id: string;
  title: string;
  body: string;
  status: StepStatus;
  summary?: string;
  outcome?: StepOutcome;
  proof?: unknown;
  transcript_summary?: string;
};

type Plan = {
  title: string;
  prelude: string;
  steps: PlanStep[];
  updatedAt: string;
};

type CompletionSignal = {
  step_id?: string;
  outcome: StepOutcome;
  summary: string;
  proof: unknown;
  findings?: unknown;
  decisions?: unknown;
  transcript_summary?: string;
  open_questions?: unknown;
  artifacts?: unknown;
};

type ToolResult = {
  role: "tool";
  call_id: string;
  content: string;
  is_error: boolean;
};

let plan: Plan = {
  title: "Untitled plan",
  prelude: "",
  steps: [],
  updatedAt: new Date().toISOString(),
};
let nextStepId = 1;
let pendingCompletion: CompletionSignal | null = null;

const SYSTEM_PROMPT =
  "You are an agentic coding assistant. Use shell_run to execute bash. " +
  "If a command's output goes quiet before it finishes, decide whether to " +
  "keep waiting (shell_wait) or stop it (shell_kill). Shell state (cwd, env, " +
  "functions) persists across commands.\n\n" +
  "You are running inside a rudimentary structured agentic loop. Maintain a " +
  "typed plan with the `plan_update` tool. The plan is living state: update " +
  "titles, bodies, ordering, and the current step whenever reality changes. " +
  "No work should happen outside an active step; if there is no suitable " +
  "active step, update the plan first. When the active step is complete, call " +
  "`task_complete` with an outcome, summary, and concrete proof. A separate " +
  "referee model will approve or decline the completion. On approval, your " +
  "conversation context is cleared and you are restarted with the whole plan " +
  "(each step title, body, and completed proof) plus the current location.";

function _okResult(call_id: string, content: string): ToolResult {
  return { role: "tool", call_id, content, is_error: false };
}

function _errResult(call_id: string, err: unknown): ToolResult {
  return {
    role: "tool",
    call_id,
    content: String((err && (err as Error).message) || err),
    is_error: true,
  };
}

function now(): string {
  return new Date().toISOString();
}

function normalizeStep(raw: any, idx: number): PlanStep {
  const status = ["pending", "active", "completed", "abandoned"].includes(
    raw && raw.status,
  )
    ? raw.status
    : "pending";
  return {
    id:
      typeof raw?.id === "string" && raw.id.length > 0
        ? raw.id
        : `step-${nextStepId++}`,
    title:
      typeof raw?.title === "string" && raw.title.length > 0
        ? raw.title
        : `Step ${idx + 1}`,
    body: typeof raw?.body === "string" ? raw.body : "",
    status,
    summary: typeof raw?.summary === "string" ? raw.summary : undefined,
    outcome: ["succeeded", "partial", "failed", "abandoned"].includes(
      raw?.outcome,
    )
      ? raw.outcome
      : undefined,
    proof: raw?.proof,
    transcript_summary:
      typeof raw?.transcript_summary === "string"
        ? raw.transcript_summary
        : undefined,
  };
}

function normalizePlan(): void {
  let sawActive = false;
  for (const step of plan.steps) {
    if (step.status === "active") {
      if (sawActive) step.status = "pending";
      sawActive = true;
    }
  }
  if (!sawActive) {
    const next = plan.steps.find((s) => s.status === "pending");
    if (next) next.status = "active";
  }
  plan.updatedAt = now();
}

function currentStep(): PlanStep | null {
  return plan.steps.find((s) => s.status === "active") || null;
}

function stepNumber(step: PlanStep): number {
  return plan.steps.findIndex((s) => s.id === step.id) + 1;
}

function textToString(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

function truncateForTranscript(value: unknown, max = 4000): string {
  const text = textToString(value);
  if (text.length <= max) return text;
  return text.slice(0, max) + `\n… [truncated ${text.length - max} chars]`;
}

function recordStepTranscript(label: string, content: unknown, max?: number): void {
  const text = truncateForTranscript(content, max).trim();
  if (!text) return;
  stepTranscriptEntries.push(`## ${label}\n${text}`);
}

function proofToString(proof: unknown): string {
  if (proof === undefined || proof === null || proof === "") return "(none)";
  if (typeof proof === "string") return proof;
  return JSON.stringify(proof, null, 2);
}

async function summarizeStepTranscript(signal: CompletionSignal): Promise<string> {
  if (stepTranscriptSummary) return stepTranscriptSummary;

  const transcriptText = stepTranscriptEntries.join("\n\n---\n\n");
  if (!transcriptText.trim()) {
    stepTranscriptSummary = "No transcript entries were captured for this step.";
    return stepTranscriptSummary;
  }

  const step = currentStep();
  const summarizer = new ChatSession({
    model_intents: ["cheap", "chat"],
    ephemeral: true,
  });
  summarizer.push({
    role: "system",
    content:
      "You summarize one completed workflow step for future agent context. " +
      "Be concise but specific. Preserve important file paths, commands, tool results, " +
      "errors, decisions, and facts learned. Do not invent details. Return only the summary.",
  });
  summarizer.push({
    role: "user",
    content:
      `Current step: ${step ? `${step.title} (id: ${step.id})` : "(none)"}\n` +
      `Step body: ${step?.body || "(empty)"}\n\n` +
      `Completion signal:\n${JSON.stringify(signal, null, 2)}\n\n` +
      `Transcript to summarize:\n${truncateForTranscript(transcriptText, 30000)}`,
  });

  try {
    const response = await summarizer.stream();
    const completed = await response.completed;
    stepTranscriptSummary =
      typeof completed.text === "string" && completed.text.trim()
        ? completed.text.trim()
        : truncateForTranscript(transcriptText, 2000);
  } catch (_) {
    stepTranscriptSummary = truncateForTranscript(transcriptText, 2000);
  }
  return stepTranscriptSummary;
}

function resetStepTranscript(): void {
  stepTranscriptEntries.length = 0;
  stepTranscriptSummary = null;
}

async function pipeAssistantTextToFrame(
  text: ReadableStream<string>,
  out: MarkdownFrame,
): Promise<string> {
  const reader = text.getReader();
  const writer = out.writable.getWriter();
  const chunks: string[] = [];
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      await writer.write(value);
    }
    await writer.close();
  } catch (err) {
    try {
      await writer.abort(err);
    } catch (_) {
      // Ignore abort failures; surface the original stream error.
    }
    throw err;
  } finally {
    reader.releaseLock();
  }
  const textOut = chunks.join("");
  recordStepTranscript("Assistant", textOut);
  return textOut;
}

function renderPlanForPrompt(): string {
  normalizePlan();
  const lines = [
    `Plan: ${plan.title}`,
    plan.prelude ? `Prelude:\n${plan.prelude}` : "Prelude: (none)",
    "",
    "Steps:",
  ];
  if (plan.steps.length === 0) {
    lines.push("  (no steps yet — create a plan before doing work)");
  }
  for (let i = 0; i < plan.steps.length; i++) {
    const step = plan.steps[i];
    lines.push(`  ${i + 1}. [${step.status}] ${step.title} (id: ${step.id})`);
    lines.push(`     body: ${step.body || "(empty)"}`);
    if (step.status === "completed") {
      lines.push(`     outcome: ${step.outcome || "succeeded"}`);
      if (step.summary) lines.push(`     summary: ${step.summary}`);
      lines.push(`     proof: ${proofToString(step.proof)}`);
      if (step.transcript_summary) {
        lines.push(`     transcript_summary: ${step.transcript_summary}`);
      }
    }
  }
  const active = currentStep();
  lines.push("");
  lines.push(
    active
      ? `Current location: step ${stepNumber(active)} (${active.id}) — ${active.title}`
      : "Current location: no active step",
  );
  return lines.join("\n");
}

function renderPlanForTool(): string {
  return renderPlanForPrompt();
}

function activateStep(ref: unknown): void {
  let target: PlanStep | undefined;
  if (typeof ref === "number") {
    target = plan.steps[ref - 1];
  } else if (typeof ref === "string") {
    target = plan.steps.find((s) => s.id === ref || s.title === ref);
  }
  if (!target) return;
  for (const step of plan.steps) {
    if (step.status === "active") step.status = "pending";
  }
  if (target.status !== "completed" && target.status !== "abandoned") {
    target.status = "active";
  }
}

function markCompletion(signal: CompletionSignal): PlanStep {
  normalizePlan();
  const step = signal.step_id
    ? plan.steps.find((s) => s.id === signal.step_id) || currentStep()
    : currentStep();
  if (!step) throw new Error("task_complete called with no active step");
  step.status = "completed";
  step.outcome = signal.outcome;
  step.summary = signal.summary;
  step.proof = signal.proof;
  step.transcript_summary = signal.transcript_summary;
  const next = plan.steps.find((s) => s.status === "pending");
  if (next) next.status = "active";
  plan.updatedAt = now();
  return step;
}

function contextAfterCompletion(completed: PlanStep): string {
  return (
    `The referee approved completion of step ${stepNumber(completed)}: ` +
    `${completed.title}. The prior conversation context has been cleared.\n\n` +
    "Here is the full living plan state. It includes all steps with title, " +
    "body, and proof for completed steps, plus the current location. Continue " +
    "from there. If the plan is finished, report the final result to the user.\n\n" +
    renderPlanForPrompt()
  );
}

const PLAN_UPDATE_SCHEMA = {
  type: "object",
  properties: {
    title: { type: "string" },
    prelude: {
      type: "string",
      description: "Durable context for the whole plan.",
    },
    steps: {
      type: "array",
      description:
        "Full ordered replacement list of steps. Each step is {id?, title, body, status?, summary?, outcome?, proof?, transcript_summary?}.",
      items: {
        type: "object",
        properties: {
          id: { type: "string" },
          title: { type: "string" },
          body: { type: "string" },
          status: {
            type: "string",
            enum: ["pending", "active", "completed", "abandoned"],
          },
          summary: { type: "string" },
          outcome: {
            type: "string",
            enum: ["succeeded", "partial", "failed", "abandoned"],
          },
          proof: {},
          transcript_summary: {
            type: "string",
            description:
              "Workflow-generated summary of the transcript for this step.",
          },
        },
        required: ["title", "body"],
      },
    },
    current_step: {
      description:
        "Optional 1-based step number, step id, or exact title to mark active.",
    },
  },
};

class PlanUpdate {
  name = "plan_update";
  description =
    "Create or update the living typed plan. The plan is in-memory for this workflow. " +
    "Call this before doing work if there is no active step, and any time the plan should change.";
  parameters = PLAN_UPDATE_SCHEMA;

  handler = async ({ call }: any) => {
    try {
      const args = call.arguments || {};
      if (typeof args.title === "string") plan.title = args.title;
      if (typeof args.prelude === "string") plan.prelude = args.prelude;
      if (Array.isArray(args.steps)) {
        plan.steps = args.steps.map((step: any, idx: number) =>
          normalizeStep(step, idx),
        );
      }
      if (args.current_step !== undefined) activateStep(args.current_step);
      normalizePlan();
      transcript.push(
        new MarkdownFrame({
          content: `Plan updated.\n\n\`\`\`\n${renderPlanForPrompt()}\n\`\`\``,
          sender: "frances",
          closed: true,
        }),
      );
      return _okResult(call.id, renderPlanForTool());
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

const TASK_COMPLETE_SCHEMA = {
  type: "object",
  properties: {
    step_id: {
      type: "string",
      description: "Optional id of the active step being completed.",
    },
    outcome: {
      type: "string",
      enum: ["succeeded", "partial", "failed", "abandoned"],
    },
    summary: { type: "string", description: "What happened in this step." },
    proof: {
      description:
        "Concrete proof: commands run, exit codes, test/build output, diffs, or explicit prose proof.",
    },
    findings: { description: "Optional facts learned during the step." },
    decisions: { description: "Optional decisions made during the step." },
    open_questions: { description: "Optional unresolved questions." },
    artifacts: { description: "Optional files, commands, turn notes, etc." },
    transcript_summary: {
      type: "string",
      description:
        "Workflow-populated summary of the step transcript; agents normally omit this.",
    },
  },
  required: ["outcome", "summary", "proof"],
};

class TaskComplete {
  name = "task_complete";
  description =
    "Signal that the active plan step is complete. Include outcome, summary, and proof. " +
    "This does not directly advance the plan; a referee model must approve it first.";
  parameters = TASK_COMPLETE_SCHEMA;

  handler = async ({ call }: any) => {
    const args = call.arguments || {};
    if (!currentStep()) {
      return _errResult(call.id, "no active step; call plan_update first");
    }
    pendingCompletion = {
      step_id: typeof args.step_id === "string" ? args.step_id : undefined,
      outcome: args.outcome,
      summary: args.summary,
      proof: args.proof,
      findings: args.findings,
      decisions: args.decisions,
      open_questions: args.open_questions,
      transcript_summary:
        typeof args.transcript_summary === "string"
          ? args.transcript_summary
          : undefined,
      artifacts: args.artifacts,
    };
    return _okResult(
      call.id,
      "Task-completion signal recorded; awaiting referee approval.",
    );
  };
}

const APPROVE_SCHEMA = { type: "object", properties: {} };
const DECLINE_SCHEMA = {
  type: "object",
  properties: {
    message: {
      type: "string",
      description:
        "Why the completion/proof is insufficient, and what the main agent should do next.",
    },
  },
  required: ["message"],
};

async function referee(signal: CompletionSignal): Promise<
  | { type: "approve" }
  | { type: "decline"; message: string }
> {
  let decision: { type: "approve" } | { type: "decline"; message: string } | null =
    null;
  const approveTool = {
    name: "approve",
    description:
      "Approve the task-completion signal because the outcome and proof satisfy the current step.",
    parameters: APPROVE_SCHEMA,
    handler: async ({ call }: any) => {
      decision = { type: "approve" };
      return _okResult(call.id, "approved");
    },
  };
  const declineTool = {
    name: "decline",
    description:
      "Decline the task-completion signal and explain what the main agent must do before trying again.",
    parameters: DECLINE_SCHEMA,
    handler: async ({ call }: any) => {
      decision = {
        type: "decline",
        message: String(call.arguments?.message || "proof was insufficient"),
      };
      return _okResult(call.id, "declined");
    },
  };
  const judge = new ChatSession({
    model_intents: ["referee", "cheap"],
    ephemeral: true,
  });
  judge.tools.push(approveTool, declineTool);
  judge.push({
    role: "system",
    content:
      "You are a strict but lightweight referee for an agentic coding loop. " +
      "Decide whether the submitted task-completion signal satisfies the " +
      "current step. You MUST call exactly one tool: approve, or decline with " +
      "a concise message telling the main agent what to do next. Do not ask the user.",
  });
  judge.push({
    role: "user",
    content:
      "Current plan state:\n\n" +
      renderPlanForPrompt() +
      "\n\nTask-completion signal:\n\n" +
      JSON.stringify(signal, null, 2),
  });

  for (let i = 0; i < 3; i++) {
    const r = await judge.stream();
    const { tool_calls } = await r.completed;
    if (decision) return decision;
    if (!tool_calls || tool_calls.length === 0) {
      judge.push({
        role: "user",
        content: "You must call approve or decline(message).",
      });
    }
  }
  return {
    type: "decline",
    message: "referee did not approve; continue and provide clearer proof",
  };
}

const sh = new Shell();
const wait = new Wait(sh);
const kill = new Kill(sh);
const editor = new Editor();
const fileSearch = new FileSearch();
const stepTranscriptEntries: string[] = [];
let stepTranscriptSummary: string | null = null;
const vars = new Variables();
const tools = [
  new Run(sh, { wait, kill }),
  wait,
  kill,
  new ShellSet(sh, vars),
  new ShellCapture(sh, vars),
  new Read(editor, vars),
  new Replace(editor, vars),
  new InsertAfter(editor, vars),
  new InsertBefore(editor, vars),
  new New(editor, vars),
  new Overwrite(editor, vars),
  new Search(fileSearch, vars),
  new VarGet(vars),
  new VarSet(vars),
  new VarAssign(vars),
  new PlanUpdate(),
  new TaskComplete(),
];

for (const tool of tools) {
  const original = tool.handler.bind(tool);
  tool.handler = async ({ call, scope }: any) => {
    recordStepTranscript(
      `Tool call: ${call.name}`,
      `id: ${call.id}\narguments:\n${textToString(call.arguments)}`,
      4000,
    );
    const result = await original({ call, scope });
    recordStepTranscript(
      `Tool result: ${call.name}`,
      `id: ${call.id}\nis_error: ${Boolean(result?.is_error)}\ncontent:\n${textToString(result?.content)}`,
      8000,
    );
    return result;
  };
}

function newAgentChat(seed?: string): ChatSession {
  const session = new ChatSession({ model_intents: ["chat"] });
  session.push({ role: "system", content: SYSTEM_PROMPT });
  session.tools.push(...tools);
  if (seed) session.push({ role: "user", content: seed });
  return session;
}

let chat = newAgentChat();

async function handlePendingCompletion(): Promise<boolean> {
  if (!pendingCompletion) return false;
  const signal = pendingCompletion;
  pendingCompletion = null;
  const judgement = await referee(signal);
  if (judgement.type === "decline") {
    const msg = `Referee declined task completion: ${judgement.message}`;
    transcript.push(new MarkdownFrame({ content: msg, closed: true }));
    chat.push({
      role: "user",
      content:
        msg +
        "\n\nContinue in the current context. Update the plan if needed, do the missing work, and call task_complete again only when the proof is adequate.",
    });
    return false;
  }

  let completed: PlanStep;
  try {
    signal.transcript_summary = await summarizeStepTranscript(signal);
    completed = markCompletion(signal);
  } catch (err) {
    chat.push({
      role: "user",
      content:
        `Internal plan error while completing the task: ${String(err)}. ` +
        "Call plan_update to repair the plan, then continue.",
    });
    return false;
  }
  transcript.push(
    new MarkdownFrame({
      content:
        `Step ${stepNumber(completed)} completed: **${completed.title}**\n\n` +
        `Outcome: ${completed.outcome}\n\n` +
        `Proof:\n\n\`\`\`\n${proofToString(completed.proof)}\n\`\`\`\n\n` +
        `Transcript summary:\n\n${completed.transcript_summary || "(none)"}`,
      sender: "frances",
      closed: true,
    }),
  );
  resetStepTranscript();
  chat = newAgentChat(contextAfterCompletion(completed));
  return true;
}

async function runAgentUntilIdle(): Promise<void> {
  let resetCount = 0;
  while (true) {
    const r = await chat.stream();
    // Push the `frances:` frame eagerly with no content — the TUI tracks
    // the id but defers measure / render, and the daemon skips persistence,
    // until the first text delta materialises the block. A tool_call-only
    // round therefore leaves no empty `frances:` row in the transcript.
    const out = new MarkdownFrame({ sender: "frances" });
    transcript.push(out);
    await pipeAssistantTextToFrame(r.text, out);
    const { tool_calls } = await r.completed;
    if (pendingCompletion) {
      const reset = await handlePendingCompletion();
      if (reset) {
        resetCount += 1;
        if (resetCount > 50) {
          transcript.push(
            new ErrorFrame({
              content:
                "agentic loop stopped after 50 automatic step transitions",
            }),
          );
          break;
        }
        continue;
      }
    }
    if (!tool_calls || tool_calls.length === 0) break;
  }
}

transcript.push(
  new MarkdownFrame({
    content: "frances ready. Type `quit` to exit.",
    closed: true,
  }),
);

try {
  for await (const input of inbox) {
    const msg = input.content.trim();
    transcript.push(
      new MarkdownFrame({ content: msg, sender: "you", closed: true }),
    );
    recordStepTranscript("User", msg);
    if (msg === "quit") {
      transcript.push(
        new MarkdownFrame({ content: "bye", sender: "frances", closed: true }),
      );
      exit();
      break;
    }

    chat.push({ role: "user", content: msg });
    try {
      await runAgentUntilIdle();
    } catch (e) {
      transcript.push(new ErrorFrame({ content: `chat failed: \`${e}\`` }));
    }
  }
} finally {
  await sh.close();
}

