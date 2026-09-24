# MCP protocol competency gaps

Status: design notes. These are missing or incomplete contracts in the Frances
MCP extension drafts, not an inventory of native harness tools or a claim that
the proposed features are implemented.

The relevant specifications are the [base protocol](model-content-hooks-protocol.md),
[skills extension](model-content-hooks-skills.md),
[UI extension](model-content-hooks-ui.md), and
[database extension](model-content-hooks-db.md).

## Host tool inventory discovery

A controller can select operation URIs when replacing context, but the portable
method for discovering the host's available tools remains unspecified.

Define how providers discover operation identities, descriptions, schemas, and
capabilities across native and MCP tools. Distinguish the available inventory
from the tools selected for the current context. Discovery must not grant
permission to execute a tool or silently change the fixed context tool set.

This is already an open question in the base protocol and is needed to make
controller-selected tool sets usable without host-specific knowledge.

## Context pressure and usage signals

Providers can request context replacement but have no specified signal that the
model context is nearly full. Define what usage and remaining-capacity information
the host exposes, distinguishing estimates from provider-reported measurements,
and when a controller gets an opportunity to preserve state.

Deliberate context replacement and host compaction need separate semantics.
Specify what happens when the host must compact, how it notifies the controller,
and how active skills and required instructions survive. Ordinary chat needs a
host fallback when no controller is active or a controller cannot respond in time.

The host must not silently discard required context or leave the session unable
to continue because a provider never requested replacement.

## Background events that wake the agent

The core hooks spec now defines provider broadcasts, a durable background outbox,
publication receipts, and delivery through the host. Events such as "the build
finished" use those primitives rather than a separate notification extension.
Evaluation stacks prevent recursive delivery, including through queued events.

Complete the wire schemas and validate ordering and recovery across background
delivery, context transitions, and pending user interactions. Notifications must
not become independent server-to-host execution requests.

Event delivery and permission to run are separate decisions. The host can record
an event while execution remains interrupted, blocked on approval, or awaiting
user input. An event must not silently restart an interrupted agent or override
new user input. A provider publishing an event does not acquire controller
authority.

## Skill activation and hook ownership

The core hooks spec now provides up-front URI advertisements, dynamic idempotent
subscriptions, retained pending intent, and full catalog updates. The subscription
key is `(MCP, hook URI)`; providers manage their own multiple handlers.

What remains is the connection to skill activation: notify providers of activation
changes and fence results from ended activations. A provider must coordinate
subscriptions shared by several skills. Ending one skill must not remove another
skill's subscription or disable mandatory host policy. This does not require a
second hook-registration mechanism.

Removal stops future delivery; it does not undo completed effects. Specify what
happens to an invocation already in progress and prevent a delayed result from
an ended activation from gaining authority when the same skill is activated again.

## Workspace and environment discovery

Providers need an unambiguous description of the workspace roots and execution
environments involved in a session. A path on an MCP server and the same path on
the host are different resources.

Join the base protocol's workspace URI binding with multi-root and remote-target
semantics. Define identities, root mappings, and the environment information a
provider can discover without assuming access to the host's filesystem or
credentials. Discovery describes an environment; it does not authorize access.

This is already partly identified in the base protocol's unresolved workspace
binding and operation-authority questions.

## Recoverable event history

Hooks provide live evidence, and the base protocol specifies stable event IDs
and deduplication. It does not fully specify how a provider catches up on events
it missed or obtains earlier evidence it needs after recovery.

Define a cursor-based replay contract or an explicitly scoped history resource.
Specify event ordering, retention boundaries, unavailable ranges, and which
providers may read which evidence. Recovery must distinguish a complete replay
from a gap; a provider must not assume missing events never occurred.

The database extension gives providers somewhere to store evidence. It does
not supply evidence they missed. History access must remain an explicit host
surface, not access to Frances's private database, local transcript paths, or
model reasoning traces.

## Execution status and budget visibility

The specs mention host-owned budgets but do not define a provider-facing account
of execution status or budget exhaustion.

Define how providers distinguish running, waiting for permission, waiting for
user input, interrupted, and budget-exhausted execution. Specify which limits
and usage measurements are exposed and how exhaustion is reported. Providers
should be able to preserve state and explain why work stopped without parsing
host-generated prose.

Budget information is observational. It does not let a provider raise its own
limits, bypass a pause, or treat elapsed time as user approval.

## Existing coverage and boundaries

Questions, approvals, progress, artifacts, durable provider storage, and model
sampling already have primitives in the drafts. Complete those contracts rather
than introducing overlapping extensions.

Sub-agent execution is a native harness tool capability. Supporting it does not
by itself require a separate MCP extension. The same applies to filesystem,
shell, search, and other ordinary tool capabilities; their existence is distinct
from the generic discovery, authorization, and lifecycle contracts above.

Planning, memory, review, and iteration can emerge from skills, tools, hooks,
and provider-owned state. They do not require dedicated protocol objects merely
because a harness can support those behaviors.

## Suggested priority

1. Host tool inventory discovery, context-pressure handling, and skill activation
   integration with hooks: close gaps in behavior the current drafts require.
2. Background wake-up and event replay: define reliable asynchronous delivery
   and recovery together.
3. Workspace/environment discovery and execution/budget visibility: complete
   the host information available to providers. Workspace identity must be
   settled before claiming interoperable multi-root or remote-target support.

These are priorities for design, not a decision to defer or remove requirements.
