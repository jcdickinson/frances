# Approval and discussion

Status: replacement design, not yet implemented. This document replaces the
earlier mandatory per-step gate and host-owned planning UI proposal. The
[UI specification](../../model-content-hooks-ui.md) defines the interaction
contract; the [core protocol](../../model-content-hooks-protocol.md) defines
context transitions and authorization.

## Plan approval

The planning MCP server owns the plan and its revisions. Frances renders semantic
UI and routes actual user decisions; it does not implement plan-specific gates.

1. The server publishes the plan as an inspectable artifact.
2. When the agent requests planning exit, the server publishes a blocking review
   bound to that exact artifact revision.
3. The host presents the review, inline or as a modal as requested and supported.
4. The user approves, requests changes, discusses, or cancels.
5. The server validates and records the action. Approval can produce a fresh
   execution context with its fixed tool selection and `next: "run"`.

Publishing a review returns promptly; it does not hold a tool call open until a
human answers. User decisions arrive through `frances/ui/respond`. The host
does not need an additional "continue" message to apply an accepted transition.

A change to the proposal requires a new review revision. Approval of an earlier
revision does not authorize the changed plan. Closing a modal, losing the
connection, or interrupting the agent is not approval.

## Discussion

Discussion keeps the decision unresolved. The user can ask questions, propose a
different approach, or clarify constraints without cancelling the review.
The controller can replace context with a planning/discussion context and carry
the relevant evidence forward.

The server records resulting decisions in its durable plan and publishes a new
artifact/review when the proposal changes. A conversational "sounds good" is
not an inferred UI approval. The user explicitly resolves the review.

A modal must let the user reach the conversation after choosing discussion.
Returning from chat does not discard the pending interaction.

## Step completion and referee review

The current JS workflow already reviews completion proof through a separate model
call. In the replacement, the planning server requests generic MCP sampling from
the host and interprets the result itself.

Approval advances the server-owned plan; rejection keeps the step active and
supplies feedback. Either can lead to explicit context replacement. Summarization
is also server-owned logic using sampling. The host does not know whether an
isolated model call is a referee, summarizer, or another workflow operation.

The server may choose to request user review at particular step boundaries.
Neither a gate after every step nor a special "full-auto" mode is mandatory
host behavior. Automatic step progression does not bypass an outstanding user
review of the plan.

## Other interactions

A choice or form can collect planning decisions before there is a complete plan.
Custom answers and discussion are distinct actions, not fake enumerated options.
Progress views and plan panels remain inspectable without requiring a user answer.

Tool authorization is separate. An approval server can participate through
`tool/permission` while the planning server controls context. A workflow's
"Approve and start" action cannot impersonate or override host permissions.

## Rendering and recovery

Use native semantic renderers through `frances/ui`, not HTML or a JS workflow
callback. Local entity state represents the rendered view, not the authoritative
workflow decision. The server persists interaction state and accepted action IDs;
the host persists delivery/transition receipts and user-visible history.

Reconnection uses the same persisted MCP session ID. Repeated answers are
deduplicated; stale revisions are rejected. Pending reviews remain pending
through host restarts and context replacement. An accepted answer received near
an interruption must not restart the model until the user resumes.

## Earlier proposal

The old four-action per-step gate, automatic patch-proposal pass, and dedicated
host plan editor are not requirements of the replacement architecture. A server
may implement equivalent workflow behavior using the semantic interaction types,
sampling, tools, and context transitions. No host plan tables or embedded JS
runtime are needed to do so.
