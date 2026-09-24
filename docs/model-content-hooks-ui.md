# Model Content + Hooks Protocol: UI

Status: draft proposal, not implemented. Extension version: 1.

This companion to the [core protocol](model-content-hooks-protocol.md) defines
`frances/ui`: interactions and artifacts rendered by the host. Servers specify
what the user needs to see or decide. The host owns layout, accessibility,
keyboard behavior, window management, and visual styling.

The extension is independent of Frances's frontend and of any particular model.
It does not provide HTML, CSS, JavaScript, arbitrary component trees, or executable
UI callbacks. A desktop app and a terminal host can implement the same interaction
semantics without displaying identical interfaces.

All wire shapes below are proposed by this document. Fields use camelCase; RPC
methods use the `frances/ui/` namespace. Requirements use the core draft's MUST,
SHOULD, and MAY conventions. This is a design contract, not yet a complete set of
JSON Schemas.

## Scope

| Primitive | Purpose | Examples |
| --- | --- | --- |
| Choice | Select one or several alternatives | Implementation approach, dependency choice |
| Form | Collect related typed values | Branch name, constraints, configuration |
| Review | Decide on a specific proposal revision | Approve a plan, request changes |
| Progress | Observe ongoing work | Current step, completed work, review pending |
| Artifact | Inspect a document or structured result | Plan, diff, code, test results |

Choices, forms, and reviews are interactions with a response lifecycle. Progress
and artifacts are persistent views; updating them does not ask the user to decide
anything. A review can refer to an artifact, but the review and artifact remain
separate objects.

All five primitives are part of this draft. Hosts advertise the types they
implement. Unsupported required interactions MUST remain unresolved; the server
must receive an explicit unsupported result rather than an invented answer.

## Capability negotiation

Use the core protocol's version intersection rules. A host advertisement might be:

```json
{
  "capabilities": {
    "experimental": {
      "frances/ui": {
        "versions": [1],
        "interactions": ["choice", "form", "review"],
        "views": ["progress", "artifact"],
        "presentations": ["inline", "modal", "panel"],
        "artifacts": ["markdown", "code", "diff", "plan", "testResults"]
      }
    }
  }
}
```

Servers advertise the same capability with the versions and features they can
use. Features are usable only when supported by both sides. `frances/ui` does not
grant permission to replace model context, execute tools, or answer host approval
prompts. Those authorities remain separate.

Each server can publish UI over its existing MCP connection. Objects and actions
are scoped by server identity and MCP session ID, so two servers can use the same
object ID without collision. Servers that need resumable UI MUST retain the
session ID and authoritative UI state until explicit deletion, as described for
durable sessions in the core protocol.

## Presentation and content

`presentation` is `inline`, `modal`, or `panel`:

- `inline`: show in the conversation near the associated work.
- `modal`: show a focused decision surface over the current view.
- `panel`: show an inspectable persistent view alongside the conversation.

Presentation never changes the meaning of an answer. A modal close, Escape key,
window close, timeout, or transport disconnect MUST NOT count as approval.
Dismissal can leave an interaction pending; only an explicit cancel action cancels
it. A host that cannot provide the requested presentation reports it as unsupported.
The server can then publish an alternative with equivalent decision semantics.

Explanatory content is plain text or constrained Markdown. Hosts do not interpret
raw HTML, scripts, style directives, embedded web applications, or executable
links. Opening links or resources is a host-mediated action. Renderers for code,
diffs, plans, and test results use structured data or immutable resource content.

Every interaction has a title. Options, fields, and actions have readable labels
and stable IDs. Color, ordering, or an icon alone cannot convey meaning. A
recommended option may be indicated, but recommendation or focus is not a user
response and MUST NOT submit automatically.

The host visibly identifies the requesting server. A workflow review is distinct
from the host's command, filesystem, or network permission UI; servers cannot
impersonate that UI through labels or presentation.

## Publishing objects

### `frances/ui/publish` — server to host

Publish a complete object snapshot with `id`, `revision`, `presentation`, and
`body`. IDs are opaque strings; revisions are positive integers that increase
when the object's content or state changes. Retransmitting the same ID and
revision with identical content is idempotent. Different content at the same
revision, or a stale revision, is an error. Updates replace snapshots, not patches.

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "frances/ui/publish",
  "params": {
    "id": "approach",
    "revision": 1,
    "presentation": "inline",
    "body": {
      "type": "choice",
      "state": "pending",
      "title": "How should we solve this problem?",
      "selection": "single",
      "options": [
        { "id": "frobinator", "label": "Use a frobinator", "recommended": true },
        { "id": "bar", "label": "Use bar" },
        { "id": "baz", "label": "Use baz" }
      ],
      "allowCustom": true,
      "allowDiscussion": true,
      "allowCancel": true,
      "blocking": true
    }
  }
}
```

The host responds promptly with `{ "status": "accepted" }` or
`{ "status": "unsupported", "reason": "..." }`. Acceptance means the host has
accepted the snapshot for presentation, not that the user has answered. Invalid
objects return an RPC error. A backgrounded app may queue presentation, but cannot
claim that queued content has been reviewed.

Publication MUST NOT hold a tool call or hook open waiting for a human. It is a
short request; user responses arrive through a separate RPC. This permits normal
chat and interruption while an interaction exists.

## Interaction types

### Choice

A choice has `selection: "single" | "multiple"` and an ordered `options` array.
Each option has a unique `id`, `label`, optional explanatory `description`, and
optional `recommended` flag. Multiple selection can specify `minSelections` and
`maxSelections`.

`allowCustom` offers a free-text alternative. A custom response is distinct from
selecting listed options; it does not manufacture an option ID. `allowDiscussion`
offers a separate discussion action. The host supplies suitable labels for custom
entry and discussion, so these are not fake options embedded in the option list.

### Form

A form contains an ordered `fields` array. Each field has an `id`, `label`,
`required` flag, optional description, and one type: `text`, `integer`, `number`,
`boolean`, or `choice`. Type-appropriate constraints include text length, numeric
bounds, and enumerated choices. Field visibility and validation are declarative;
there are no scripts or expressions. Host validation improves feedback; the
server validates the submitted values as well.

Forms collect ordinary workflow values, not credentials. Secret entry and
credential storage remain host facilities, outside these form fields.

### Review

A review binds a decision to an immutable artifact revision. Example publication:

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "frances/ui/publish",
  "params": {
    "id": "approve-plan",
    "revision": 1,
    "presentation": "modal",
    "body": {
      "type": "review",
      "state": "pending",
      "title": "Approve the implementation plan",
      "artifact": { "id": "plan", "revision": 7 },
      "approveLabel": "Approve and start",
      "allowChanges": true,
      "allowDiscussion": true,
      "allowCancel": true,
      "blocking": true
    }
  }
}
```

The host must be able to present the referenced artifact before enabling approval.
The reference is scoped to the publishing server and session. It cannot mean
"whatever the latest plan happens to be when the button is clicked."

Actions are approve, request changes with feedback, discuss, and cancel. Approval
binds to both the interaction revision and artifact revision. Any substantive
proposal change requires a new review revision and a fresh explicit approval.
The host never silently transfers approval to a newer artifact revision.

## Responses and discussion

### `frances/ui/respond` — host to server

The host sends the exact object revision shown to the user and a stable `actionId`:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "frances/ui/respond",
  "params": {
    "id": "approve-plan",
    "revision": 1,
    "actionId": "action-8",
    "action": {
      "type": "approve",
      "artifact": { "id": "plan", "revision": 7 }
    }
  }
}
```

| Action type | Payload | Meaning |
| --- | --- | --- |
| `select` | `optionIds` | Resolve a choice with listed options |
| `custom` | `text` | Resolve a choice with the user's alternative |
| `submit` | `values`, keyed by field ID | Resolve a form |
| `approve` | Exact artifact reference | Approve that proposal revision |
| `requestChanges` | Exact artifact reference and `feedback` | Reject the proposal as presented and request revision |
| `discuss` | Optional opening `text` | Suspend resolution and enter discussion |
| `cancel` | Optional `reason` | Explicitly cancel the interaction |

Only actions supported by the interaction are valid. The host sends actual user
input, not an answer inferred by the model. The server checks IDs, revisions,
constraints, and its current workflow state before accepting an action.

The response is `{ "status": "accepted" }` or
`{ "status": "rejected", "reason": "...", "currentRevision": 2 }`.
The server commits acceptance before acknowledging it and deduplicates retries by
`actionId`. Reusing an action ID for different content is an error. A lost response
is retried with the same action ID rather than submitting the decision again.

Accepted actions produce a new published object revision. States are `pending`,
`discussing`, `resolved`, and `cancelled`. While an answer is awaiting acknowledgment,
the host shows that fact and prevents conflicting submissions. Rejection does not
resolve the interaction. The host refreshes the current snapshot before retrying.

**Discussion is not an answer or cancellation.** An accepted `discuss` action
keeps the question unresolved and opens a conversation associated with that
interaction. The host includes `{ id, revision }` under
`_meta["frances/ui"]` on the associated `prompt/submit` event. The controller can
seed a fresh discussion context using `frances/context`, or continue the current
conversation if its existing tools and instructions are appropriate.

Messages in that discussion inform the server's revisions; they are not implicit
approval. The server can publish a revised question or review in `pending` state.
A modal must allow the user to reach chat after selecting discussion. Closing the
discussion leaves the decision unresolved unless the user explicitly answers or
cancels it.

## Blocking and execution

`blocking: true` means the workflow cannot continue past the interaction until the
server has accepted a resolving action and the corresponding state is published.
The host pauses automatic continuation at the next safe boundary. Already-started
tool effects are not rolled back; the server must publish a required review before
initiating work that depends on its approval.

Discussion permits model turns addressing the unresolved question, while dependent
execution remains blocked. The controller is responsible for selecting an
appropriate planning/discussion context and its fixed tool set. The host does not
infer workflow permissions from the text of a question. `blocking: false` allows
unrelated work to continue without treating the question as answered.

Accepting `requestChanges` or `cancel` resolves that request, but does not approve
dependent work. Approval itself also does not auto-run the model. The controller
must explicitly request continuation or context replacement. A response to
`frances/ui/respond` may carry a transition in `_meta["frances/context"]` using
the core transition contract, only when sent by the selected controller with the
negotiated capability. Other UI providers cannot obtain controller authority by
returning such metadata.

The host includes its current `contextId` and `revision` under
`_meta["frances/context"]` in a response request to the controller. An interrupt
or new user input can invalidate automatic continuation even if the server has
already durably recorded the answer. The core reconciliation rules still apply.

## Persistent views

Progress views contain a title, optional explanatory text, and ordered items with
stable IDs and states such as pending, active, completed, skipped, or failed.
They can show an indeterminate status instead of an invented percentage. They
have no approval action. Cancelling work is an explicit host/controller operation,
not a side effect of closing the progress panel.

Artifacts have a kind, title, and either inline data or a resource reference to
immutable revision content. The initial kinds are Markdown, code, diff, plan, and
test results. Typed plans can display a prelude, ordered steps, status, summaries,
and proof. Hosts may display an explicitly supplied Markdown alternative when
the structured kind is unsupported; they must not drop decision-relevant content.

Artifact revisions referenced by reviews MUST remain retrievable for the review's
lifetime and audit history. Updating the plan panel to revision 8 does not replace
revision 7 in an existing review. The server must explicitly supersede that review
if revision 7 should no longer be approvable, and reject a stale approval even if
the update has not yet reached the host.

## Reconnection and removal

The server owns authoritative UI objects and accepted actions. The host may cache
rendered snapshots and retain interaction history, but cannot invent server state
after a disconnect. Pending user responses may be queued durably for retry and
remain visibly unconfirmed until accepted.

`frances/ui/list` is a host-to-server request with empty parameters returning
`{ "objects": [...] }`, the current published snapshots. On reconnect, the host
loads this snapshot, reconciles pending action IDs, and then enables interaction.
This uses the saved MCP session ID, not a separate UI attachment protocol.

`frances/ui/read` accepts `{ id, revision }` and returns that immutable snapshot,
including older artifact revisions referenced by reviews. Missing revisions are
errors; the host must not replace them with the latest version.

`frances/ui/remove` is a server-to-host request with `{ id, revision }` naming a
new revision that retires the object; its result is empty. Removal is idempotent
and is retained as a tombstone against delayed publications. A pending interaction
must first be explicitly cancelled or superseded; removing UI is never approval.
Removal hides the live surface but does not erase the user's decision history.

If the connection cannot deliver server-to-host publication, the provider cannot
use live UI on that connection. It must report the limitation, not claim the user
has seen or answered an interaction. During a disconnect, blocking interactions
remain blocking.

## Plan approval flow

Frances will supply a planning MCP server ported from the
[recorded main workflow](arch/main-workflow.md). This approval flow is an explicit
addition to that port. First Frances will remove the JS runtime and become an
ordinary harness without MCP; the server and this UI integration follow later.

1. The planning server maintains the plan through its ordinary MCP tools and
   publishes an inspectable plan artifact.
2. When the agent requests planning exit, the server publishes a blocking review
   of that exact plan revision. Publishing returns promptly; execution waits.
3. The user approves, requests changes, discusses, or cancels through native UI.
4. Discussion keeps the decision unresolved and allows a planning conversation.
   Revisions produce a new artifact and review; stale actions are rejected.
5. On approval, the server records the decision and resolves the review. Its
   response proposes a new execution context containing the approved plan and
   the execution tool selection, with `next: "run"`.
6. The host applies that transition at a safe boundary. No additional "continue"
   message is needed. An interruption still takes precedence over automatic run.

The existing workflow's immediate `plan_exit` becomes request-for-review followed
by an explicit user decision. Referee completion checks remain model judgments;
they do not substitute for user approval of a plan.

## Frances implementation fit

Frances can render snapshots through its existing
[entity system](arch/session-runtime.md#entities): transcript references for inline
objects, persistent panels for artifacts/progress, and native interaction views
for choices and reviews. The Rust runtime routes protocol actions; Svelte renders
known semantic types. Neither layer needs to load server-supplied executable UI.

The current entity lifecycle force-settles live entities when their producer dies.
Pending server-owned interactions cannot inherit that as a decision: after host
restart, reconcile authoritative server snapshots and restore unresolved requests.
A settled rendering entity is not an approved or cancelled workflow interaction.

## Remaining schema decisions

- Complete JSON Schemas for form constraints, progress items, and artifact kinds.
- Precise constrained Markdown profile and resource-loading limits.
- Pagination and retention for large UI snapshots and historical revisions.
- Multiple simultaneous blocking interactions and modal queue ordering.
- How non-controller UI providers route discussion into the controller's workflow.

These are specification questions, not permission to silently omit unsupported
required interactions or treat dismissal as approval.

## Review scenarios

1. A single-choice question offers recommendations, a custom answer, and discussion
   without encoding the latter two as ordinary options.
2. The same question works inline or as a modal with identical response semantics.
3. A modal close or disconnect leaves plan approval unresolved.
4. Approval of plan revision 7 cannot approve revision 8, including during a race.
5. Discussion permits revising the plan and returning to review without cancelling
   or accidentally approving it.
6. Replayed actions and publications are idempotent; stale revisions are rejected.
7. Pending interactions survive context replacement and transport reconnection.
8. An approval recorded just before interruption does not restart the agent.
9. Unsupported required UI blocks dependent continuation and reports the limitation.
10. Multiple servers can publish views without sharing object identities or gaining
    controller authority; host permission prompts remain distinct.
11. Plan approval starts execution through an explicit context transition whose
    tools remain fixed for the new context.
