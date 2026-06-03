// Frances primary workflow: an agentic chat with shell and file tools.
// User input becomes a user message; the LLM responds and can call
// shell_run/wait/kill against one long-lived bash subprocess, plus the
// anchor-aware file_read/replace/insert_*/new/overwrite family.
//
// This version also keeps a rudimentary in-memory agentic loop:
//   - a planning phase that interviews the user, then `plan_exit`
//   - `plan_begin` to drop back into planning mid-execution to ask the user
//     questions, carrying a goal + self-context across the context clear
//   - a living typed plan (`plan_update`)
//   - a structured task-completion signal (`task_complete`)
//   - a cheap/referee model that approves or declines completion
//   - ralph-wiggum reset after every referee verdict (approve or decline):
//     the context is cleared and re-seeded from the plan state
//
// Wire up as the daemon's default workflow via config:
//   default_workflow = "main"
//
//   [workflows.main]
//   id = "<uuid>"
//   file = "assets/workflows/main.ts"
//
// Type `quit` to exit.

import { inbox, INTERRUPT } from "frances:v1/inbox";
import { AbortController } from "whatwg:abortcontroller";
import {
  transcript,
  MarkdownSection,
  ErrorSection,
  ReasoningSection,
  ToolUseSection,
} from "frances:v1/sections";
import { ChatSession, complete } from "frances:v1/chat";
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
  ReplaceLines,
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
import { exit, setStatus } from "frances:v1/workflow";
import { envBlock, cwdBlock } from "frances:v1/context-sections";
import { toolGuidance } from "frances:v1/tool-family";
import {
  globalAgents,
  localAgents,
  nestedAgentsInventory,
} from "frances:v1/agent-sections";

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
let pendingPlanExit = false;
// When set, the execution agent has asked to drop back into planning. `goal`
// is what it wants to resolve with the user; `context` is self-context it
// wants carried across the context clear. Both seed the planning conversation.
type PlanBeginRequest = { goal: string; context: string };
let pendingPlanBegin: PlanBeginRequest | null = null;

type Mode = "planning" | "executing";
let mode: Mode = "planning";

// The planning interview is adapted from Matt Pocock's "grill-me" skill:
// https://github.com/mattpocock/skills/blob/main/skills/productivity/grill-me/SKILL.md
const PLANNING_PROMPT =
  "You are an agentic coding assistant, currently in the PLANNING phase. Your " +
  "job here is not to do the work — it is to reach a shared understanding with " +
  "the user and capture it as a typed plan.\n\n" +
  "Interview the user relentlessly about every aspect of this plan until you " +
  "reach a shared understanding. Walk down each branch of the design tree, " +
  "resolving dependencies between decisions one-by-one. For each question, " +
  "provide your recommended answer.\n\n" +
  "Ask the questions one at a time.\n\n" +
  "If a question can be answered by exploring the codebase, explore the " +
  "codebase instead (you have read and search tools).\n\n" +
  "As understanding crystallises, record it with the `plan_update` tool: a " +
  "title, a prelude capturing durable context and the decisions reached, and " +
  "an ordered list of steps. The plan should read like an ADR. When you and " +
  "the user share a clear understanding and the plan has concrete steps, call " +
  "`plan_exit` to leave planning and begin execution.";

const SYSTEM_PROMPT =
  "You are an agentic coding assistant in the EXECUTION phase of a structured " +
  "agentic loop. A plan has already been agreed with the user. The plan is " +
  "living state: keep it accurate with the `plan_update` tool whenever reality " +
  "changes, but don't re-plan from scratch. No work should happen outside an " +
  "active step. When the active step is complete, call `task_complete` with an " +
  "outcome, summary, and concrete proof. A separate referee model will approve " +
  "or decline the completion. On approval, your conversation context is cleared " +
  "and you are restarted with the whole plan (each step title, body, and " +
  "completed proof) plus the current location. If you are blocked on something " +
  "only the user can decide, call `plan_begin` to return to planning and ask — " +
  "don't guess. Occasionally provide updates on what you are trying to do; " +
  "don't just present the user with a stream of tool calls.";

// Stable section objects for planning and execution prompts.
// These are mode-specific identity objects referenced by promptSections.
const planningPromptSection = {
  name: "planning-prompt",
  prompt(_ctx: any): string {
    return PLANNING_PROMPT;
  },
};

const executionPromptSection = {
  name: "execution-prompt",
  prompt(_ctx: any): string {
    return SYSTEM_PROMPT;
  },
};

// Shared section list: envBlock (immutable, front), mode prompt, toolGuidance,
// agent discovery sections, cwdBlock (mutable, late). Each mode swaps the
// mode-specific section; the rest are shared.
const baseSections = [
  envBlock,
  toolGuidance,
  globalAgents,
  localAgents,
  nestedAgentsInventory,
  cwdBlock,
];
const planningSections = [planningPromptSection, ...baseSections];
const executionSections = [executionPromptSection, ...baseSections];

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

function recordStepTranscript(
  label: string,
  content: unknown,
  max?: number,
): void {
  const text = truncateForTranscript(content, max).trim();
  if (!text) return;
  stepTranscriptEntries.push(`## ${label}\n${text}`);
}

function proofToString(proof: unknown): string {
  if (proof === undefined || proof === null || proof === "") return "(none)";
  if (typeof proof === "string") return proof;
  return JSON.stringify(proof, null, 2);
}

async function summarizeStepTranscript(
  signal: CompletionSignal,
): Promise<string> {
  if (stepTranscriptSummary) return stepTranscriptSummary;

  const transcriptText = stepTranscriptEntries.join("\n\n---\n\n");
  if (!transcriptText.trim()) {
    stepTranscriptSummary =
      "No transcript entries were captured for this step.";
    return stepTranscriptSummary;
  }

  const step = currentStep();
  const summarizer = new ChatSession({
    model_intents: ["cheap", "chat"],
    ephemeral: true,
  });
  summarizer.promptSections.push({
    name: "summarizer",
    prompt() {
      return "You summarize one completed workflow step for future agent context. " +
        "Be concise but specific. Preserve important file paths, commands, tool results, " +
        "errors, decisions, and facts learned. Do not invent details. Return only the summary.";
    },
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
  out: MarkdownSection,
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

// Reasoning channel: pipe straight to a ReasoningSection.
//
// Two different model-feedback loops to keep straight:
//   1. Provider history round-trip — reasoning still rides back to the
//      model on the next assistant turn as `ContentPart::ReasoningContent`
//      (genai builds that payload from the chunks; we don't touch it).
//   2. Frances's step summariser (`recordStepTranscript` →
//      `summarizeStepTranscript` → `renderPlan`) — this is the
//      one we *deliberately* exclude reasoning from. Reasoning is bulky
//      model-internal scratch work; folding it into the step summary
//      would dilute the summary and burn tokens for no real benefit.
async function pipeReasoningToFrame(
  reasoning: ReadableStream<string>,
  out: ReasoningSection,
): Promise<void> {
  const reader = reasoning.getReader();
  const writer = out.writable.getWriter();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      await writer.write(value);
    }
    await writer.close();
  } catch (err) {
    try {
      await writer.abort(err);
    } catch (_) {}
    throw err;
  } finally {
    reader.releaseLock();
  }
}

// Render the plan as an ADR-style markdown document: a title, prelude prose,
// and one section per step. This is what the user sees in the transcript and
// what the model and referee read back in their prompts.
function renderPlan(): string {
  normalizePlan();
  const out = [`# ${plan.title}`];
  if (plan.prelude.trim()) out.push("", plan.prelude.trim());
  if (plan.steps.length === 0) {
    out.push("", "_No steps yet — agree a plan before doing work._");
    return out.join("\n");
  }
  for (let i = 0; i < plan.steps.length; i++) {
    const step = plan.steps[i];
    out.push("", `## ${i + 1}. ${step.title} _(${step.status})_`);
    if (step.body.trim()) out.push("", step.body.trim());
    if (step.status === "completed") {
      out.push("", `**Outcome:** ${step.outcome || "succeeded"}`);
      if (step.summary) out.push("", `**Summary:** ${step.summary}`);
      out.push("", "**Proof:**", "", "```", proofToString(step.proof), "```");
      if (step.transcript_summary) {
        out.push("", `**Transcript summary:** ${step.transcript_summary}`);
      }
    }
  }
  const active = currentStep();
  out.push(
    "",
    active
      ? `**Current step:** ${stepNumber(active)}. ${active.title}`
      : "**Current step:** none",
  );
  return out.join("\n");
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
    renderPlan()
  );
}

function executionSeed(): string {
  return (
    "Planning is complete. The agreed plan is below. Begin executing from the " +
    "active step. Work only that step; when it is done call `task_complete` " +
    "with an outcome, summary, and concrete proof.\n\n" +
    renderPlan()
  );
}

function planBeginSeed(req: PlanBeginRequest): string {
  const goal = req.goal || "(no goal stated)";
  const context = req.context
    ? `Context you carried forward:\n\n${req.context}\n\n`
    : "";
  return (
    "You have returned to PLANNING from execution to resolve something with " +
    "the user. Your execution context has been cleared.\n\n" +
    `What you need to resolve:\n\n${goal}\n\n` +
    context +
    "Interview the user to resolve it. Record the resolution in the plan with " +
    "`plan_update` (in the prelude or the relevant step body) so it survives " +
    "the context clear; completed steps keep their proof, so do not redo or " +
    "discard them. When the plan is right, call `plan_exit` to resume " +
    "execution from the active step.\n\n" +
    renderPlan()
  );
}

function declineSeed(reason: string): string {
  return (
    "A referee reviewed your `task_complete` signal for the active step and " +
    "DECLINED it. Your conversation context has been cleared. Reason given:\n\n" +
    reason +
    "\n\nThe full plan and your current position are below. Do the missing " +
    "work for the active step, then call `task_complete` again only once the " +
    "proof is adequate.\n\n" +
    renderPlan()
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

  describe(call: any) {
    const a = call.arguments || {};
    if (typeof a.title === "string" && a.title.length > 0) return a.title;
    if (a.current_step !== undefined) return `→ ${a.current_step}`;
    return "";
  }

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
        new MarkdownSection({
          content: `**Plan updated.**\n\n${renderPlan()}`,
          source: "assistant",
          closed: true,
        }),
      );
      return _okResult(call.id, renderPlan());
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

  describe(call: any) {
    const a = call.arguments || {};
    return typeof a.outcome === "string" ? a.outcome : "";
  }

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

class PlanExit {
  name = "plan_exit";
  description =
    "Leave the planning phase and begin executing the plan. Call this only " +
    "once you and the user share a clear understanding and the plan has " +
    "concrete steps.";
  parameters = { type: "object", properties: {} };

  describe() {
    return "";
  }

  handler = async ({ call }: any) => {
    if (plan.steps.length === 0) {
      return _errResult(
        call.id,
        "cannot exit planning with an empty plan; call plan_update to add steps first",
      );
    }
    pendingPlanExit = true;
    return _okResult(call.id, "Planning complete; beginning execution.");
  };
}

const PLAN_BEGIN_SCHEMA = {
  type: "object",
  properties: {
    goal: {
      type: "string",
      description:
        "What you need to resolve with the user, and why it blocks the current step.",
    },
    context: {
      type: "string",
      description:
        "Self-context to carry across the context clear: what you have " +
        "discovered, tried, or decided so far that your planning self will need.",
    },
  },
};

class PlanBegin {
  name = "plan_begin";
  description =
    "Return to the planning phase to ask the user questions or reshape the " +
    "plan when you are blocked on something only the user can resolve. Your " +
    "execution context is cleared, so state the `goal` and any `context` worth " +
    "keeping. Call `plan_exit` to resume execution once it is resolved.";
  parameters = PLAN_BEGIN_SCHEMA;

  describe(call: any) {
    const a = call.arguments || {};
    return typeof a.goal === "string" ? a.goal : "";
  }

  handler = async ({ call }: any) => {
    const args = call.arguments || {};
    pendingPlanBegin = {
      goal: typeof args.goal === "string" ? args.goal.trim() : "",
      context: typeof args.context === "string" ? args.context.trim() : "",
    };
    return _okResult(call.id, "Returning to planning to resolve with the user.");
  };
}

const DECIDE_SCHEMA = {
  type: "object",
  properties: {
    verdict: {
      type: "string",
      enum: ["approve", "decline"],
      description:
        "`approve` if the outcome and proof satisfy the current step, otherwise `decline`.",
    },
    message: {
      type: "string",
      description:
        "When declining: why the completion/proof is insufficient, and what the main agent should do next.",
    },
  },
  required: ["verdict"],
};

async function referee(
  signal: CompletionSignal,
): Promise<{ type: "approve" } | { type: "decline"; message: string }> {
  // One forced `decide` call — `complete` (with `toolChoice`) routes to
  // the enforced path, so the force-a-tool + scold-on-miss + bounded
  // retry loop lives in Rust, not here.
  let r;
  try {
    r = await complete({
      intents: ["referee", "cheap"],
      input: [
        {
          role: "system",
          content:
            "You are a strict but lightweight referee for an agentic coding loop. " +
            "Decide whether the submitted task-completion signal satisfies the " +
            "current step. Call the `decide` tool exactly once: verdict " +
            '"approve", or "decline" with a concise `message` telling the main ' +
            "agent what to do next. Do not ask the user.",
        },
        {
          role: "user",
          content:
            "Current plan state:\n\n" +
            renderPlan() +
            "\n\nTask-completion signal:\n\n" +
            JSON.stringify(signal, null, 2),
        },
      ],
      tools: [
        {
          name: "decide",
          description:
            "Approve or decline the task-completion signal for the current step.",
          parameters: DECIDE_SCHEMA,
        },
      ],
      toolChoice: "decide",
      maxToolCalls: 1,
    });
  } catch (e: any) {
    // Enforcement gave up (or the call failed): default to decline so the
    // loop continues rather than falsely approving.
    return {
      type: "decline",
      message: `referee unavailable (${String((e && e.message) || e)}); continue and provide clearer proof`,
    };
  }

  const call = (r.tool_calls || []).find((c: any) => c.name === "decide");
  const verdict = call?.arguments?.verdict;
  if (verdict === "approve") return { type: "approve" };
  if (verdict === "decline") {
    return {
      type: "decline",
      message: String(call.arguments?.message || "proof was insufficient"),
    };
  }
  return {
    type: "decline",
    message: "referee did not produce a verdict; continue and provide clearer proof",
  };
}

const sh = new Shell();
const wait = new Wait(sh);
const kill = new Kill(sh);
const stepTranscriptEntries: string[] = [];
let stepTranscriptSummary: string | null = null;
const vars = new Variables();

// Wrap a tool's handler to mirror its calls/results into the transcript and the
// step summary. Each tool must be wrapped exactly once — wrapping is not
// idempotent (it captures the current handler), so only ever wrap a fresh
// instance.
function wrapTool(tool: any): void {
  const original = tool.handler.bind(tool);
  tool.handler = async ({ call, scope }: any) => {
    transcript.push(new ToolUseSection({ call, tool }));
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

// Persistent tools: the long-lived shell subprocess, the variable store, and
// the global plan all outlive any single context, so these are built and
// wrapped once.
const run = new Run(sh, { wait, kill });
const shellSet = new ShellSet(sh, vars);
const shellCapture = new ShellCapture(sh, vars);
const varGet = new VarGet(vars);
const varSet = new VarSet(vars);
const varAssign = new VarAssign(vars);
const planUpdate = new PlanUpdate();
const taskComplete = new TaskComplete();
const planExit = new PlanExit();
const planBegin = new PlanBegin();
const persistentTools = [
  run,
  wait,
  kill,
  shellSet,
  shellCapture,
  varGet,
  varSet,
  varAssign,
  planUpdate,
  taskComplete,
  planExit,
  planBegin,
];
for (const tool of persistentTools) wrapTool(tool);

// Per-context file tools. A fresh `Editor` owns this context's read state
// ("have I read this here?"); the `FileSearch` bound to it shares the loop
// guard. `freshContext()` rebuilds them — and the planning/execution tool
// lists — against a new Editor, so reads reset whenever context clears.
let editor: any;
let planningTools: any[];
let executingTools: any[];

function freshContext(): void {
  editor = new Editor();
  const fileSearch = new FileSearch(editor);
  const read = new Read(editor, vars);
  const search = new Search(fileSearch, vars);
  const editTools = [
    new ReplaceLines(editor, vars),
    new InsertAfter(editor, vars),
    new InsertBefore(editor, vars),
    new New(editor, vars),
    new Overwrite(editor, vars),
  ];
  for (const tool of [read, search, ...editTools]) wrapTool(tool);

  const explore = [
    run,
    wait,
    kill,
    shellSet,
    shellCapture,
    read,
    search,
    varGet,
    varSet,
    varAssign,
  ];
  planningTools = [...explore, planUpdate, planExit];
  executingTools = [...explore, ...editTools, planUpdate, taskComplete, planBegin];
}

function newPlanningChat(seed?: string): ChatSession {
  freshContext();
  const session = new ChatSession({ model_intents: ["chat"] });
  session.promptSections.push(...planningSections);
  session.tools.push(...planningTools);
  if (seed) session.push({ role: "user", content: seed });
  return session;
}

function newExecutionChat(seed?: string): ChatSession {
  freshContext();
  const session = new ChatSession({ model_intents: ["chat"] });
  session.promptSections.push(...executionSections);
  session.tools.push(...executingTools);
  if (seed) session.push({ role: "user", content: seed });
  return session;
}

let chat = newPlanningChat();

async function handlePendingCompletion(): Promise<boolean> {
  if (!pendingCompletion) return false;
  const signal = pendingCompletion;
  pendingCompletion = null;
  const judgement = await referee(signal);
  if (judgement.type === "decline") {
    // Ralph Wiggum retry: clear context and restart the step from the plan,
    // carrying only the referee's reason forward. Same reset as an approval;
    // the difference is the step doesn't advance and the step transcript
    // keeps accumulating across the retry (so the eventual summary covers it).
    const msg = `Referee declined task completion: ${judgement.message}`;
    transcript.push(new MarkdownSection({ content: msg, closed: true }));
    chat = newExecutionChat(declineSeed(judgement.message));
    return true;
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
    new MarkdownSection({
      content:
        `Step ${stepNumber(completed)} completed: **${completed.title}**\n\n` +
        `Outcome: ${completed.outcome}\n\n` +
        `Proof:\n\n\`\`\`\n${proofToString(completed.proof)}\n\`\`\`\n\n` +
        `Transcript summary:\n\n${completed.transcript_summary || "(none)"}`,
      source: "assistant",
      closed: true,
    }),
  );
  resetStepTranscript();
  chat = newExecutionChat(contextAfterCompletion(completed));
  return true;
}

// Single outstanding `inbox.next()`. NEVER create a second one: a
// leaked promise keeps its Rust future alive and would consume-and-drop
// a real input. `turn()` races *this same promise*; `ourLoop()` is the
// only place that consumes it and re-arms.
let pending = inbox.next();

// Why the turn ended. `idle` = the model stopped calling tools (done
// for now); `interjected` = a new inbox item arrived mid-turn and is
// sitting in `pending` for `ourLoop` to handle next.
type TurnEnd = "idle" | "interjected";

// Drive the agentic loop for one user turn, racing each model round
// against the live inbox so the user can interrupt or interject
// mid-stream. On a mid-round inbox event we abort the in-flight stream
// and roll chat history back to a clean boundary (dropping the partial
// assistant turn + any orphaned tool_calls — keeping the OpenAI
// contract that tool_calls are immediately answered).
async function turn(): Promise<TurnEnd> {
  // Bound the automatic loop: every context reset (step advance / retry) and
  // every nudge counts against the same budget. Returns true once the budget
  // is blown, having surfaced the error — the caller should stop the loop.
  let resetCount = 0;
  function overBudget(): boolean {
    resetCount += 1;
    if (resetCount <= 50) return false;
    transcript.push(
      new ErrorSection({
        content:
          "agentic loop stopped after 50 automatic iterations (resets + nudges)",
      }),
    );
    return true;
  }

  while (true) {
    setStatus(mode === "planning" ? "planning…" : "working…");
    const cp = await chat.checkpoint();
    const ac = new AbortController();
    const r = await chat.stream({ maxToolCalls: 8, signal: ac.signal });
    // Push the `frances:` frame eagerly with no content — the TUI tracks
    // the id but defers measure / render, and the daemon skips
    // persistence, until the first text delta materialises the block.
    // The `thought` frame sits alongside it for the reasoning channel;
    // for non-thinking models it stays empty and closes immediately.
    const out = new MarkdownSection({ source: "assistant" });
    const thought = new ReasoningSection();
    transcript.push(thought);
    transcript.push(out);

    const round = (async () => {
      await Promise.all([
        pipeAssistantTextToFrame(r.text, out),
        pipeReasoningToFrame(r.reasoning, thought),
      ]);
      return await r.completed;
    })();

    const winner = await Promise.race([
      round.then((completed) => ({ kind: "round" as const, completed })),
      pending.then(() => ({ kind: "inbox" as const })),
    ]);

    if (winner.kind === "inbox") {
      // Interrupt or interjection arrived mid-round. Abort the stream,
      // wait for it to settle, then roll history back to the clean
      // checkpoint. `pending` is left for `ourLoop` to consume.
      ac.abort(new Error("interrupted"));
      // Esc means "stop". Aborting only tears down the chat stream — the
      // shell command runs on its own subprocess, decoupled so it can
      // survive quiet handoffs. SIGKILL it so its handler settles instead
      // of streaming into the next command's frame, and so bash returns to
      // idle rather than rejecting the next shell_run as busy. The kill
      // also makes the quiet-negotiation loop's `isRunning()` guard false,
      // so awaiting `r.completed` below can't spin up a stray round.
      try {
        await sh.kill();
      } catch (_) {
        // Nothing in flight, or the shell is already closed — fine.
      }
      try {
        await round;
      } catch (_) {
        // Expected: the aborted stream rejects with the abort reason.
      }
      // Let the dispatch finish: handlers close their frames and compute
      // their results. We await it BEFORE rolling back so those orphaned
      // tool_results are inside the checkpoint window and get discarded,
      // rather than landing in history after the truncation point.
      try {
        await r.completed;
      } catch (_) {
        // Aborted before the round settled — nothing dispatched.
      }
      await chat.rollback(cp);
      setStatus(null);
      return "interjected";
    }

    const { tool_calls } = winner.completed;
    if (pendingPlanExit) {
      pendingPlanExit = false;
      mode = "executing";
      resetStepTranscript();
      transcript.push(
        new MarkdownSection({
          content: "Planning complete — beginning execution.",
          source: "assistant",
          closed: true,
        }),
      );
      chat = newExecutionChat(executionSeed());
      continue;
    }
    if (pendingPlanBegin) {
      // Execution agent dropped back into planning to ask the user. Clear
      // context and switch toolsets; the plan (with its completion statuses)
      // is preserved, so `plan_exit` later just resumes from the active step.
      // Counting this against the budget bounds any plan_begin/plan_exit
      // ping-pong (only one side needs to count to bound the round-trip).
      const req = pendingPlanBegin;
      pendingPlanBegin = null;
      if (overBudget()) break;
      mode = "planning";
      transcript.push(
        new MarkdownSection({
          content: "Returning to planning to resolve a question with the user.",
          source: "assistant",
          closed: true,
        }),
      );
      chat = newPlanningChat(planBeginSeed(req));
      continue;
    }
    if (pendingCompletion) {
      const reset = await handlePendingCompletion();
      if (reset) {
        if (overBudget()) break;
        continue;
      }
    }
    if (!tool_calls || tool_calls.length === 0) {
      // The only clean way out of a step is `task_complete`. If the model went
      // idle while a step is still active, it stopped prematurely — nudge it
      // to drive the step to a completion signal rather than handing a
      // half-done step back to the user. With no active step the plan is
      // finished, so break and yield (the same as planning's Q&A pause).
      if (mode === "executing" && currentStep()) {
        if (overBudget()) break;
        chat.push({
          role: "user",
          content:
            "You stopped without finishing the active step. Continue working " +
            "on it. Call `task_complete` with an outcome (succeeded / partial " +
            "/ failed), a summary, and concrete proof once it is done or you " +
            "are blocked; call `plan_update` if the plan needs to change, or " +
            "`plan_begin` if you need to ask the user something.",
        });
        continue;
      }
      break;
    }
  }
  // Turn boundary: reconcile accumulated file edits (clears anchor
  // tombstones). The workflow owns this now — the host no longer fires
  // it per prompt.
  await editor.commit();
  setStatus(null);
  return "idle";
}

transcript.push(
  new MarkdownSection({
    content:
      "frances ready. Describe what you'd like to do and we'll plan it " +
      "together first. Type `quit` to exit.",
    closed: true,
  }),
);

// Top-level loop. `pending` is always exactly one outstanding inbox
// read; we await it, immediately re-arm, then act. `turn()` may also
// observe `pending` resolving (mid-turn) — that's fine, multiple awaits
// on one promise are safe; only this loop ever re-issues `inbox.next()`.
async function ourLoop(): Promise<void> {
  while (true) {
    const { value, done } = await pending;
    pending = inbox.next();
    if (done) break;

    if (value === INTERRUPT) {
      // At the top level there's nothing running to interrupt — the
      // current turn (if any) already aborted itself before handing
      // control back. Just wait for the next input.
      continue;
    }

    const msg = value.content.trim();
    transcript.push(
      new MarkdownSection({ content: msg, source: "user", closed: true }),
    );
    recordStepTranscript("User", msg);
    if (msg === "quit") {
      transcript.push(
        new MarkdownSection({ content: "bye", source: "assistant", closed: true }),
      );
      exit();
      break;
    }

    chat.push({ role: "user", content: msg });
    try {
      await turn();
    } catch (e) {
      transcript.push(new ErrorSection({ content: `chat failed: \`${e}\`` }));
    }
  }
}

try {
  await ourLoop();
} finally {
  await sh.close();
}
