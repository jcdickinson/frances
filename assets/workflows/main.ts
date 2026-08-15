// Frances primary workflow: an agentic chat with shell and file tools.
// User input becomes a user message; the LLM responds and can call the
// standard shell, file, search, and variable tools.
//
// The workflow maintains a planning/execution loop with:
//   - planning interviews followed by `plan_exit`
//   - atomic pending-step edits through `plan_update`
//   - sequential `plan_finish_step` completion or skipping
//   - referee review of completed work
//   - context reset and reseeding after every progression or decline
//   - `plan_begin` when execution requires a user decision
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
  ErrorSection,
  ReasoningSection,
  ToolUseSection,
} from "frances:v1/sections";
import { postMessage, openMessage } from "frances:v1/messages";
import { ChatSession, complete, loadChatSession } from "frances:v1/chat";
import { db } from "frances:v1/storage";
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

type StepStatus = "pending" | "active" | "completed" | "skipped";

type PlanStep = {
  title: string;
  body: string;
  status: StepStatus;
  summary?: string;
  proof?: unknown;
  reason?: string;
  transcript_summary?: string;
};

type Plan = {
  title: string;
  prelude: string;
  steps: PlanStep[];
  updatedAt: string;
};

type FinishSignal =
  | {
      action: "complete";
      summary: string;
      proof: unknown;
      transcript_summary?: string;
    }
  | { action: "skip"; reason: string };

type ToolResult = {
  role: "tool";
  call_id: string;
  content: string;
  is_error: boolean;
};


type PersistedChat = {
  id: number | null;
  mode: Mode;
  pendingSeed: string | null;
};

type PersistedStepTranscript = {
  entries: string[];
  summary: string | null;
};

type PersistedState = {
  schemaVersion: 3;
  instanceId: string;
  mode: Mode;
  plan: Plan;
  effort: number | null;
  currentChat: PersistedChat;
  variables: Array<[string, unknown]>;
  stepTranscript: PersistedStepTranscript;
  pending: {
    completion: FinishSignal | null;
    planExit: boolean;
    planBegin: PlanBeginRequest | null;
  };
};

const STATE_SCHEMA_VERSION = 3;
const INSTANCE_ID = String(import.meta.instance);
const STATE_TABLE = "main_workflow_state";

let currentChatId: number | null = null;
let currentChatPendingSeed: string | null = null;
let effortOverride: number | null = null;

let plan: Plan = {
  title: "Untitled plan",
  prelude: "",
  steps: [],
  updatedAt: new Date().toISOString(),
};
let pendingCompletion: FinishSignal | null = null;
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
  "I am an agentic coding assistant, currently in the PLANNING phase. My " +
  "job here is not to do the work — it is to reach a shared understanding with " +
  "the user and capture it as a typed plan.\n\n" +
  "I will interview the user relentlessly about every aspect of this plan until I " +
  "reach a shared understanding. I will walk down each branch of the design tree, " +
  "resolving dependencies between decisions one-by-one. For each question, " +
  "I will provide my recommended answer.\n\n" +
  "I will ask the questions one at a time.\n\n" +
  "If a question can be answered by exploring the codebase, I will explore the " +
  "codebase instead (I have read and search tools).\n\n" +
  "As understanding crystallises, I will record it with the `plan_update` tool: a " +
  "title, a prelude capturing durable context and decisions, and atomic edits to " +
  "an ordered list of execution steps. Plans contain only work to execute after " +
  "planning; interviewing, investigating to form the plan, and planning itself are " +
  "not steps. I write the plan like an ADR and in first person: the prelude records " +
  "what I know, and each step says what I will do. When the user and I share a clear " +
  "understanding and the plan has concrete execution steps, I MUST call `plan_exit` " +
  "to leave planning and begin execution.";

const SYSTEM_PROMPT =
  "I am an agentic coding assistant in the EXECUTION phase of a structured " +
  "agentic loop. A plan has already been agreed with the user. The plan contains " +
  "only execution work. I will keep pending steps accurate with atomic `plan_update` " +
  "operations whenever reality changes, but completed/skipped history and the active " +
  "step are protected. I will work only on the active step. When it is done, I MUST " +
  "call `plan_finish_step` with either `complete` plus summary/proof, or `skip` plus " +
  "a reason. Completion is referee-reviewed; skipping advances directly. On advance, " +
  "my conversation context is cleared and I am restarted from the whole plan and " +
  "current location. If blocked on something only the user can decide, I MUST call " +
  "`plan_begin` to return to planning and ask — I won't guess. I will occasionally " +
  "provide updates on what I am trying to do; I won't just present a stream of tool calls.";

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


async function ensureStateTable(): Promise<void> {
  await db.exec(
    `CREATE TABLE IF NOT EXISTS ${STATE_TABLE} (` +
      "instance_id TEXT PRIMARY KEY, " +
      "version INTEGER NOT NULL, " +
      "state_json TEXT NOT NULL, " +
      "updated_at TEXT NOT NULL" +
      ")",
  );
}

function makePersistedState(): PersistedState {
  return {
    schemaVersion: STATE_SCHEMA_VERSION,
    instanceId: INSTANCE_ID,
    mode,
    plan,
    effort: effortOverride,

    currentChat: {
      id: currentChatId,
      mode,
      pendingSeed: currentChatPendingSeed,
    },
    variables: vars.entries(),
    stepTranscript: {
      entries: [...stepTranscriptEntries],
      summary: stepTranscriptSummary,
    },
    pending: {
      completion: pendingCompletion,
      planExit: pendingPlanExit,
      planBegin: pendingPlanBegin,
    },
  };
}

async function saveState(): Promise<void> {
  await ensureCurrentChatPersisted();
  const state = makePersistedState();
  await db.exec(
    `INSERT INTO ${STATE_TABLE} (instance_id, version, state_json, updated_at) ` +
      "VALUES (?, ?, ?, ?) " +
      "ON CONFLICT(instance_id) DO UPDATE SET " +
      "version = excluded.version, " +
      "state_json = excluded.state_json, " +
      "updated_at = excluded.updated_at",
    [INSTANCE_ID, STATE_SCHEMA_VERSION, JSON.stringify(state), now()],
  );
}

async function loadPersistedState(): Promise<PersistedState | null> {
  await ensureStateTable();
  const rows = await db.query(
    `SELECT state_json FROM ${STATE_TABLE} WHERE instance_id = ?`,
    [INSTANCE_ID],
  );
  if (!rows || rows.length === 0) return null;
  const raw = JSON.parse(rows[0].state_json);
  if (raw?.schemaVersion !== STATE_SCHEMA_VERSION) {
    throw new Error(`unsupported main workflow state schema: ${raw?.schemaVersion}`);
  }
  return raw as PersistedState;
}

function restorePersistedState(state: PersistedState): void {
  mode = state.mode === "executing" ? "executing" : "planning";
  plan = state.plan;
  validatePlan();
  effortOverride =
    Number.isInteger(state.effort) && state.effort! >= 0 && state.effort! <= 100
      ? state.effort
      : null;
  currentChatId = typeof state.currentChat?.id === "number" ? state.currentChat.id : null;
  currentChatPendingSeed =
    typeof state.currentChat?.pendingSeed === "string"
      ? state.currentChat.pendingSeed
      : null;
  vars.replace(Array.isArray(state.variables) ? state.variables : []);
  stepTranscriptEntries.length = 0;
  if (Array.isArray(state.stepTranscript?.entries)) {
    stepTranscriptEntries.push(...state.stepTranscript.entries);
  }
  stepTranscriptSummary =
    typeof state.stepTranscript?.summary === "string"
      ? state.stepTranscript.summary
      : null;
  pendingCompletion = state.pending?.completion || null;
  pendingPlanExit = Boolean(state.pending?.planExit);
  pendingPlanBegin = state.pending?.planBegin || null;
}

function validatePlan(): void {
  let seenUnfinished = false;
  let activeCount = 0;
  for (const step of plan.steps) {
    if (!["pending", "active", "completed", "skipped"].includes(step.status)) {
      throw new Error(`invalid plan step status: ${step.status}`);
    }
    if (step.status === "completed" || step.status === "skipped") {
      if (seenUnfinished) throw new Error("terminal steps must form an immutable prefix");
    } else {
      seenUnfinished = true;
    }
    if (step.status === "active") activeCount += 1;
  }
  if (activeCount > 1) throw new Error("plan has more than one active step");
  const firstUnfinished = plan.steps.findIndex(
    (step) => step.status === "active" || step.status === "pending",
  );
  const active = currentStepIndex();
  if (active >= 0 && active !== firstUnfinished) {
    throw new Error("the active step must be the first unfinished step");
  }
}

function activateNextStep(): void {
  if (currentStep()) return;
  const next = plan.steps.find((step) => step.status === "pending");
  if (next) next.status = "active";
  plan.updatedAt = now();
}

function currentStep(): PlanStep | null {
  return plan.steps.find((step) => step.status === "active") || null;
}

function currentStepIndex(): number {
  return plan.steps.findIndex((step) => step.status === "active");
}

function stepNumber(step: PlanStep): number {
  return plan.steps.indexOf(step) + 1;
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
  signal: FinishSignal,
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
      return "I will summarize one completed workflow step for future agent context. " +
        "Be concise but specific. Preserve important file paths, commands, tool results, " +
        "errors, decisions, and facts learned. I will not invent details. I will return only the summary.";
    },
  });
  summarizer.push({
    role: "user",
    content:
      `Current step: ${step ? step.title : "(none)"}\n` +
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
  out: ReturnType<typeof openMessage>,
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
  validatePlan();
  const out = [`# ${plan.title}`];
  if (plan.prelude.trim()) out.push("", plan.prelude.trim());
  if (plan.steps.length === 0) {
    out.push("", "_No execution steps yet — agree a plan before doing work._");
    return out.join("\n");
  }
  for (let i = 0; i < plan.steps.length; i++) {
    const step = plan.steps[i];
    out.push("", `## ${i + 1}. ${step.title} _(${step.status})_`);
    if (step.body.trim()) out.push("", step.body.trim());
    if (step.status === "completed") {
      if (step.summary) out.push("", `**Summary:** ${step.summary}`);
      out.push("", "**Proof:**", "", "```", proofToString(step.proof), "```");
      if (step.transcript_summary) {
        out.push("", `**Transcript summary:** ${step.transcript_summary}`);
      }
    } else if (step.status === "skipped") {
      out.push("", `**Skip reason:** ${step.reason || "(none)"}`);
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

function markFinished(signal: FinishSignal): PlanStep {
  validatePlan();
  const step = currentStep();
  if (!step) throw new Error("plan_finish_step called with no active step");
  if (signal.action === "complete") {
    step.status = "completed";
    step.summary = signal.summary;
    step.proof = signal.proof;
    step.transcript_summary = signal.transcript_summary;
  } else {
    step.status = "skipped";
    step.reason = signal.reason;
  }
  activateNextStep();
  validatePlan();
  return step;
}

function contextAfterProgress(step: PlanStep): string {
  const result = step.status === "completed" ? "completed with referee approval" : "skipped";
  return (
    `Step ${stepNumber(step)} was ${result}: ${step.title}. ` +
    "The prior conversation context has been cleared.\n\n" +
    "Here is the full plan state, including terminal history and the current location. " +
    "I will continue strictly in order. If the plan is finished, I report the final result.\n\n" +
    renderPlan()
  );
}

function executionSeed(): string {
  return (
    "Planning is complete. The agreed execution plan is below. I will begin at the " +
    "active step and progress strictly in order. When it is done I MUST call " +
    "`plan_finish_step` with `complete` and summary/proof, or `skip` and a reason.\n\n" +
    renderPlan()
  );
}

function planBeginSeed(req: PlanBeginRequest): string {
  const goal = req.goal || "(no goal stated)";
  const context = req.context
    ? `Context I carried forward:\n\n${req.context}\n\n`
    : "";
  return (
    "I have returned to PLANNING from execution to resolve something with " +
    "the user. My execution context has been cleared.\n\n" +
    `What I need to resolve:\n\n${goal}\n\n` +
    context +
    "I will interview the user to resolve it. I will record the resolution in the plan with " +
    "`plan_update` (in the prelude or the relevant step body) so it survives " +
    "the context clear; completed steps keep their proof, so I will not redo or " +
    "discard them. When the plan is right, I MUST call `plan_exit` to resume " +
    "execution from the active step.\n\n" +
    renderPlan()
  );
}

function declineSeed(reason: string): string {
  return (
    "A referee reviewed my `plan_finish_step` completion for the active step and " +
    "DECLINED it. My conversation context has been cleared. Reason given:\n\n" +
    reason +
    "\n\nThe full plan and my current position are below. I will do the missing " +
    "work for the active step, then call `plan_finish_step` with `complete` again " +
    "only once the proof is adequate.\n\n" +
    renderPlan()
  );
}

const STEP_INPUT_SCHEMA = {
  type: "object",
  properties: {
    title: { type: "string" },
    body: {
      type: "string",
      description: "Execution work, written in first person.",
    },
  },
  required: ["title", "body"],
};

const PLAN_UPDATE_SCHEMA = {
  type: "object",
  properties: {
    title: { type: "string" },
    prelude: { type: "string", description: "Durable context and decisions." },
    operations: {
      type: "array",
      description:
        "Atomic, sequential, zero-based list edits. Completed/skipped steps and the active step cannot be edited, removed, or moved.",
      items: {
        oneOf: [
          {
            type: "object",
            properties: {
              action: { type: "string", enum: ["add"] },
              index: { type: "integer", minimum: 0 },
              step: STEP_INPUT_SCHEMA,
            },
            required: ["action", "index", "step"],
          },
          {
            type: "object",
            properties: {
              action: { type: "string", enum: ["update"] },
              index: { type: "integer", minimum: 0 },
              step: STEP_INPUT_SCHEMA,
            },
            required: ["action", "index", "step"],
          },
          {
            type: "object",
            properties: {
              action: { type: "string", enum: ["remove"] },
              index: { type: "integer", minimum: 0 },
            },
            required: ["action", "index"],
          },
          {
            type: "object",
            properties: {
              action: { type: "string", enum: ["move"] },
              from: { type: "integer", minimum: 0 },
              to: { type: "integer", minimum: 0 },
            },
            required: ["action", "from", "to"],
          },
        ],
      },
    },
  },
};

function editableBoundary(steps: PlanStep[]): number {
  const active = steps.findIndex((step) => step.status === "active");
  if (active >= 0) return active + 1;
  let terminal = -1;
  for (let index = 0; index < steps.length; index++) {
    if (steps[index].status === "completed" || steps[index].status === "skipped") {
      terminal = index;
    }
  }
  return terminal + 1;
}

function requireIndex(index: unknown, length: number, label: string): number {
  if (!Number.isInteger(index) || (index as number) < 0 || (index as number) >= length) {
    throw new Error(`${label} must be a zero-based index between 0 and ${length - 1}`);
  }
  return index as number;
}

function pendingStep(raw: any): PlanStep {
  if (typeof raw?.title !== "string" || !raw.title.trim()) {
    throw new Error("step title must be a non-empty string");
  }
  if (typeof raw?.body !== "string" || !raw.body.trim()) {
    throw new Error("step body must be a non-empty execution instruction");
  }
  return { title: raw.title.trim(), body: raw.body.trim(), status: "pending" };
}

function applyPlanOperations(steps: PlanStep[], operations: any[]): PlanStep[] {
  const next = steps.map((step) => ({ ...step }));
  for (const operation of operations) {
    const boundary = editableBoundary(next);
    if (operation?.action === "add") {
      const index = operation.index;
      if (!Number.isInteger(index) || index < boundary || index > next.length) {
        throw new Error(`add index must be between ${boundary} and ${next.length}`);
      }
      next.splice(index, 0, pendingStep(operation.step));
    } else if (operation?.action === "update") {
      const index = requireIndex(operation.index, next.length, "update index");
      if (index < boundary) throw new Error(`step ${index} is protected`);
      next[index] = pendingStep(operation.step);
    } else if (operation?.action === "remove") {
      const index = requireIndex(operation.index, next.length, "remove index");
      if (index < boundary) throw new Error(`step ${index} is protected`);
      next.splice(index, 1);
    } else if (operation?.action === "move") {
      const from = requireIndex(operation.from, next.length, "move from");
      const to = requireIndex(operation.to, next.length, "move to");
      if (from < boundary || to < boundary) throw new Error("move crosses the protected boundary");
      const [step] = next.splice(from, 1);
      next.splice(to, 0, step);
    } else {
      throw new Error(`unknown plan operation: ${operation?.action}`);
    }
  }
  return next;
}

function planTitles(): string {
  return plan.steps.map((step, index) => `${index}. ${step.title}`).join("\n");
}

class PlanUpdate {
  name = "plan_update";
  description =
    "I update plan metadata and pending execution steps with atomic zero-based operations. " +
    "Operations run sequentially and all fail together. Terminal history and the active step are protected.";
  parameters = PLAN_UPDATE_SCHEMA;

  describe(call: any) {
    const operations = call.arguments?.operations;
    return Array.isArray(operations) ? `${operations.length} operation(s)` : "metadata";
  }

  handler = async ({ call }: any) => {
    try {
      const args = call.arguments || {};
      const nextTitle = typeof args.title === "string" ? args.title.trim() : plan.title;
      const nextPrelude = typeof args.prelude === "string" ? args.prelude : plan.prelude;
      if (!nextTitle) throw new Error("plan title must be non-empty");
      const nextSteps = Array.isArray(args.operations)
        ? applyPlanOperations(plan.steps, args.operations)
        : plan.steps;
      plan = {
        title: nextTitle,
        prelude: nextPrelude,
        steps: nextSteps,
        updatedAt: now(),
      };
      validatePlan();
      const confirmation = `Plan updated:\n${planTitles() || "(no steps)"}`;
      postMessage({ source: "assistant", content: confirmation });
      await saveState();
      return _okResult(call.id, confirmation);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

const PLAN_FINISH_STEP_SCHEMA = {
  type: "object",
  oneOf: [
    {
      properties: {
        action: { type: "string", enum: ["complete"] },
        summary: { type: "string", description: "What was completed." },
        proof: { description: "Concrete commands, output, diffs, or explicit prose proof." },
      },
      additionalProperties: false,
      required: ["action", "summary", "proof"],
    },
    {
      properties: {
        action: { type: "string", enum: ["skip"] },
        reason: { type: "string", description: "Why this execution step should be skipped." },
      },
      additionalProperties: false,
      required: ["action", "reason"],
    },
  ],
};

class PlanFinishStep {
  name = "plan_finish_step";
  description =
    "I finish the active step sequentially. `complete` awaits referee approval; `skip` records a reason and advances immediately. There is no target or outcome argument.";
  parameters = PLAN_FINISH_STEP_SCHEMA;

  describe(call: any) {
    return typeof call.arguments?.action === "string" ? call.arguments.action : "";
  }

  handler = async ({ call }: any) => {
    const args = call.arguments || {};
    if (!currentStep()) return _errResult(call.id, "no active step");
    if (args.action === "complete") {
      if (typeof args.summary !== "string" || !args.summary.trim()) {
        return _errResult(call.id, "complete requires a non-empty summary");
      }
      pendingCompletion = {
        action: "complete",
        summary: args.summary.trim(),
        proof: args.proof,
      };
      await saveState();
      return _okResult(call.id, "Step completion recorded; awaiting referee approval.");
    }
    if (args.action === "skip") {
      if (typeof args.reason !== "string" || !args.reason.trim()) {
        return _errResult(call.id, "skip requires a non-empty reason");
      }
      pendingCompletion = { action: "skip", reason: args.reason.trim() };
      await saveState();
      return _okResult(call.id, "Step skip recorded; advancing sequentially.");
    }
    return _errResult(call.id, "action must be complete or skip");
  };
}

class PlanExit {
  name = "plan_exit";
  description =
    "I leave the planning phase and begin executing the plan. I MUST call this only " +
    "once the user and I share a clear understanding and the plan has " +
    "concrete steps.";
  parameters = { type: "object", properties: {} };

  describe() {
    return "";
  }

  handler = async ({ call }: any) => {
    if (plan.steps.length === 0) {
      return _errResult(
        call.id,
        "cannot exit planning with an empty plan; add execution steps first",
      );
    }
    validatePlan();
    activateNextStep();
    if (!currentStep()) {
      return _errResult(call.id, "cannot resume execution because the plan is already finished");
    }
    pendingPlanExit = true;
    await saveState();
    return _okResult(call.id, "Planning complete; beginning execution.");
  };
}

const PLAN_BEGIN_SCHEMA = {
  type: "object",
  properties: {
    goal: {
      type: "string",
      description:
        "What I need to resolve with the user, and why it blocks the current step.",
    },
    context: {
      type: "string",
      description:
        "Self-context to carry across the context clear: what I have " +
        "discovered, tried, or decided so far that my planning self will need.",
    },
  },
};

class PlanBegin {
  name = "plan_begin";
  description =
    "I return to the planning phase to ask the user questions or reshape the " +
    "plan when I am blocked on something only the user can resolve. My " +
    "execution context is cleared, so I state the `goal` and any `context` worth " +
    "keeping. I MUST call `plan_exit` to resume execution once it is resolved.";
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
    await saveState();
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
  signal: FinishSignal,
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
            "I am a strict but lightweight referee for an agentic coding loop. " +
            "I will decide whether the submitted task-completion signal satisfies the " +
            "current step. I MUST call the `decide` tool exactly once: verdict " +
            '"approve", or "decline" with a concise `message` telling the main ' +
            "agent what to do next. I will not ask the user.",
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
      message: `referee unavailable (${String((e && e.message) || e)}); I will continue and provide clearer proof`,
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
    message: "referee did not produce a verdict; I will continue and provide clearer proof",
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
    await saveState();
    const result = await original({ call, scope });
    recordStepTranscript(
      `Tool result: ${call.name}`,
      `id: ${call.id}\nis_error: ${Boolean(result?.is_error)}\ncontent:\n${textToString(result?.content)}`,
      8000,
    );
    await saveState();
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
const planFinishStep = new PlanFinishStep();
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
  planFinishStep,
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
  executingTools = [...explore, ...editTools, planUpdate, planFinishStep, planBegin];
}

function attachPlanningChat(session: ChatSession): ChatSession {
  freshContext();
  session.effort = effortOverride;
  session.promptSections.push(...planningSections);
  session.tools.push(...planningTools);
  return session;
}

function attachExecutionChat(session: ChatSession): ChatSession {
  freshContext();
  session.effort = effortOverride;
  session.promptSections.push(...executionSections);
  session.tools.push(...executingTools);
  return session;
}

async function ensureCurrentChatPersisted(): Promise<void> {
  if (currentChatId !== null || !chat) return;
  const id = await chat.ensurePersisted();
  currentChatId = typeof id === "number" ? id : null;
}

async function setChat(session: ChatSession, seed?: string | null): Promise<void> {
  chat = session;
  currentChatId = null;
  currentChatPendingSeed = seed || null;
  if (seed) chat.push({ role: "user", content: seed });
  await saveState();
}

async function newPlanningChat(seed?: string): Promise<void> {
  const session = attachPlanningChat(new ChatSession({ model_intents: ["chat"] }));
  await setChat(session, seed);
}

async function newExecutionChat(seed?: string): Promise<void> {
  const session = attachExecutionChat(new ChatSession({ model_intents: ["chat"] }));
  await setChat(session, seed);
}

let chat: ChatSession;

async function loadChatForState(state: PersistedState): Promise<ChatSession> {
  const loaded = state.currentChat.id === null
    ? new ChatSession({ model_intents: ["chat"] })
    : await loadChatSession(state.currentChat.id);
  const session = state.mode === "executing"
    ? attachExecutionChat(loaded)
    : attachPlanningChat(loaded);
  if (currentChatPendingSeed) {
    session.push({ role: "user", content: currentChatPendingSeed });
  }
  return session;
}

async function initializeWorkflow(): Promise<boolean> {
  const state = await loadPersistedState();
  if (state) {
    restorePersistedState(state);
    chat = await loadChatForState(state);
    return true;
  }

  chat = attachPlanningChat(new ChatSession({ model_intents: ["chat"] }));
  currentChatId = null;
  currentChatPendingSeed = null;
  await ensureCurrentChatPersisted();
  return false;
}

async function handleEffortCommand(msg: string): Promise<boolean> {
  if (msg !== "/effort" && !msg.startsWith("/effort ")) return false;

  const parts = msg.split(/\s+/);
  let response: string;
  if (parts.length === 1) {
    response = effortOverride === null ? "default" : `${effortOverride}%`;
  } else if (parts.length === 2 && parts[1] === "default") {
    effortOverride = null;
    chat.effort = null;
    response = "Effort: default";
  } else if (
    parts.length === 2 &&
    /^(?:0|[1-9]\d?|100)$/.test(parts[1])
  ) {
    effortOverride = Number(parts[1]);
    chat.effort = effortOverride;
    response = `Effort: ${effortOverride}%`;
  } else {
    response = "Usage: /effort [default|0-100]";
  }

  postMessage({ source: "assistant", content: response });
  await saveState();
  return true;
}

async function handlePendingCompletion(): Promise<boolean> {
  if (!pendingCompletion) return false;
  const signal = pendingCompletion;
  if (signal.action === "skip") {
    const skipped = markFinished(signal);
    postMessage({
      source: "assistant",
      content: `Step ${stepNumber(skipped)} skipped: **${skipped.title}**\n\nReason: ${skipped.reason}`,
    });
    resetStepTranscript();
    pendingCompletion = null;
    await newExecutionChat(contextAfterProgress(skipped));
    return true;
  }
  const judgement = await referee(signal);
  if (judgement.type === "decline") {
    const msg = `Referee declined step completion: ${judgement.message}`;
    postMessage({ content: msg });
    pendingCompletion = null;
    await newExecutionChat(declineSeed(judgement.message));
    return true;
  }

  let completed: PlanStep;
  try {
    signal.transcript_summary = await summarizeStepTranscript(signal);
    completed = markFinished(signal);
  } catch (err) {
    chat.push({
      role: "user",
      content: `Internal plan error while completing the step: ${String(err)}. Continue the active step.`,
    });
    await saveState();
    return false;
  }
  postMessage({
    source: "assistant",
    content:
      `Step ${stepNumber(completed)} completed: **${completed.title}**\n\n` +
      `Proof:\n\n\`\`\`\n${proofToString(completed.proof)}\n\`\`\`\n\n` +
      `Transcript summary:\n\n${completed.transcript_summary || "(none)"}`,
  });
  resetStepTranscript();
  pendingCompletion = null;
  await newExecutionChat(contextAfterProgress(completed));
  return true;
}

// Single outstanding `inbox.next()`. NEVER create a second one: a
// leaked promise keeps its Rust future alive and would consume-and-drop
// a real input. `turn()` races *this same promise*; `ourLoop()` is the
// only place that consumes it and re-arms.
let pending = inbox.next();

// Why the turn ended. `idle` = the model stopped calling tools (done
// for now); `interjected` = a user message arrived mid-turn and is sitting
// in `pending` for `ourLoop` to handle as the next turn; `interrupted` =
// the user pressed Esc and we stopped the round.
type TurnEnd = "idle" | "interjected" | "interrupted";

// Drive the agentic loop for one user turn, racing each model round
// against the live inbox so the user can interrupt or interject
// mid-stream.
//
//   - Esc (INTERRUPT): abort the stream and kill the shell. Dispatch then
//     answers every still-running tool call with an "Interrupted by user"
//     result (see chat.js), so the partial assistant turn stays in history
//     well-formed — the conversation records the interruption rather than
//     discarding it.
//   - A user message mid-round (interjection): let the round finish so its
//     tool calls complete with real results, then hand back to `ourLoop`.
//     The message is still in `pending` and becomes the next turn, so the
//     model sees the tool results and the new message together.
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


  async function consumePendingSignals(): Promise<boolean> {
    if (pendingPlanExit) {
      pendingPlanExit = false;
      mode = "executing";
      resetStepTranscript();
      postMessage({
        source: "assistant",
        content: "Planning complete — beginning execution.",
      });
      await saveState();
      await newExecutionChat(executionSeed());
      return true;
    }
    if (pendingPlanBegin) {
      const req = pendingPlanBegin;
      pendingPlanBegin = null;
      if (overBudget()) return false;
      mode = "planning";
      postMessage({
        source: "assistant",
        content: "Returning to planning to resolve a question with the user.",
      });
      await saveState();
      await newPlanningChat(planBeginSeed(req));
      return true;
    }
    if (pendingCompletion) {
      const reset = await handlePendingCompletion();
      if (reset) {
        if (overBudget()) return false;
        return true;
      }
    }
    return false;
  }
  while (true) {
    if (await consumePendingSignals()) continue;
    setStatus(mode === "planning" ? "planning…" : "working…");
    const ac = new AbortController();
    const r = await chat.stream({ maxToolCalls: 8, signal: ac.signal });
    // Open the assistant message eagerly with empty text; each delta
    // refreshes its snapshot. The `thought` frame sits alongside it for
    // the reasoning channel; for non-thinking models it stays empty and
    // closes immediately.
    const out = openMessage("assistant");
    const thought = new ReasoningSection();
    transcript.push(thought);

    const round = (async () => {
      await Promise.all([
        pipeAssistantTextToFrame(r.text, out),
        pipeReasoningToFrame(r.reasoning, thought),
      ]);
      return await r.completed;
    })();

    const winner = await Promise.race([
      round.then((completed) => ({ kind: "round" as const, completed })),
      pending.then((p) => ({ kind: "inbox" as const, item: p.value })),
    ]);

    if (winner.kind === "inbox") {
      if (winner.item === INTERRUPT) {
        // Esc means "stop". Abort the chat stream and SIGKILL the shell —
        // the shell command runs on its own subprocess, decoupled so it can
        // survive quiet handoffs, so killing it settles its handler instead
        // of streaming into the next command's frame and leaving bash busy.
        // The kill also makes the quiet-negotiation loop's `isRunning()`
        // guard false, so awaiting `r.completed` can't spin up a stray
        // round. We then let dispatch settle (chat.js answers any tool call
        // the kill left unfinished with an "Interrupted by user" result)
        // and DON'T roll back: the assistant turn and its tool results stay
        // in history, recording the interruption and keeping the next
        // request well-formed.
        ac.abort(new Error("interrupted"));
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
        try {
          await r.completed;
        } catch (_) {
          // Aborted before the round settled — nothing was dispatched.
        }
        setStatus(null);
        return "interrupted";
      }
      // A user message arrived mid-round. Let the round finish so its tool
      // calls complete with real results; the message stays in `pending`
      // for `ourLoop` to handle as the next turn.
      try {
        await round;
        currentChatPendingSeed = null;
        await saveState();
      } catch (_) {
        // The round failed on its own; `ourLoop` surfaces the error.
      }
      setStatus(null);
      return "interjected";
    }

    currentChatPendingSeed = null;
    await saveState();
    const { tool_calls } = winner.completed;
    if (await consumePendingSignals()) continue;
    if (!tool_calls || tool_calls.length === 0) {
      // The only clean way out of a step is `plan_finish_step`. If the model went
      // idle while a step is still active, nudge it to continue.
      if (mode === "executing" && currentStep()) {
        if (overBudget()) break;
        chat.push({
          role: "user",
          content:
            "I stopped without finishing the active step. I MUST continue working on it. " +
            "When done I MUST call `plan_finish_step` with `complete` plus summary/proof, " +
            "or `skip` plus a reason. I MUST use `plan_update` only for pending-step edits, " +
            "or `plan_begin` if I need to ask the user something.",
        });
        await saveState();
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

const restored = await initializeWorkflow();
if (!restored) {
  postMessage({
    content:
      "frances ready. Describe what you'd like to do and we'll plan it " +
      "together first. Type `quit` to exit.",
  });
  await saveState();
}

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
    postMessage({ source: "user", content: msg });
    if (msg === "quit") {
      postMessage({ source: "assistant", content: "bye" });
      exit();
      break;
    }
    if (await handleEffortCommand(msg)) continue;

    recordStepTranscript("User", msg);
    await saveState();

    currentChatPendingSeed = msg;
    chat.push({ role: "user", content: msg });
    await saveState();
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
