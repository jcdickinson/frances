# Model Content + Hooks: Skills

Status: design proposal, not implemented. Proposed extension version: 1.
Companion to the [base protocol](model-content-hooks-protocol.md). This file
records the skills design separately while the base protocol is being edited.

## Model

A skill is an invocable entry point supplied by an MCP provider. Invocation
calls that provider, which can act on its application state and return
instructions, content, or a permitted context transition. The host discovers
skills, routes invocations, and tracks which are active. A provider can implement
a skill by returning static instructions, but invocation still calls MCP.

There are two activation modes:

| Mode | Meaning |
| --- | --- |
| `primary` | Occupies the session's single primary skill slot |
| `regular` | Can be active alongside other regular skills, with or without a primary |

**Primary skill** is the proposed name for an exclusive skill. The exclusivity
is between primary skills; it does not exclude regular skills. A session has
zero or one primary skill and any number of regular skills.

For example, `$frances` invokes a primary skill. Its instructions, tools, and
hooks produce the iterative planning behavior. While it is active, the agent
can invoke `$software_engineer` or a language-specific skill to help with the
current work. Those activations leave `$frances` active. The primary can also
register and deregister sub-skills as it continues. A sub-skill is a regular
skill available only within that primary activation; invoking it calls the
primary's MCP provider.

There is no workflow object, workflow type, or workflow lifecycle. Planning,
review, and iteration emerge from skill instructions and provider behavior.
The host does not interpret those activities as protocol states.

## What the primary slot controls

The slot belongs to the host user session, across all configured providers.
Separate per-provider application session IDs do not create separate primary
slots. Two providers cannot each activate a primary in the same host session.

The slot is durable activation state, not a mutex held while a model runs or
while a human is answering a question. It does not lock workspace files, other
host sessions, server processes, or tool execution. It provides no protection
against another session editing the same files.

Regular skills operate within the active primary's instructions and enforced
constraints. They cannot replace it, release its slot, or bypass its tool and
hook policies. Conflicting instructions do not grant additional authority.
Mandatory host instructions, permissions, and user control still apply.

The primary does not need to approve every regular activation. Existing host
policy and activated hooks still apply to model-facing activation tools. An
additional primary-owned allowlist is not proposed.

## Discovery and invocation

Propose a `frances/skills` capability with `{ "versions": [1] }`, negotiated
using the base protocol's extension rules. It requires `frances/session` for
durable provider identity, but does not by itself require hooks or context
control. Skills consisting only of instructions are valid.

Two host-to-provider methods separate discovery from execution:

- `frances/skills/list`: returns descriptors, without full instructions.
- `frances/skills/invoke`: invokes the selected skill on its MCP provider.
  Parameters include `name`, `revision`, and `input` (MCP content blocks).
  A dynamic sub-skill additionally carries its `registrationId`.

A descriptor has a provider-local `name`, an opaque `revision`, a short
`description`, and an explicit `mode`. Names and revisions are nonempty strings.
The host identifies a skill by `(configured provider identity, name)`, never by
an unqualified display name alone. Names use letters, digits, `_`, and `-`, with
no `/`; revisions are opaque. A revision identifies a skill contract, not a
cached result. Invocation results can depend on current provider state.

Illustrative list result:

```json
{
  "resultType": "complete",
  "skills": [
    {
      "name": "frances",
      "revision": "1",
      "description": "Develop a plan with the user, execute it, and review each step.",
      "mode": "primary"
    },
    {
      "name": "software_engineer",
      "revision": "1",
      "description": "Apply engineering guidance to the current task.",
      "mode": "regular"
    }
  ]
}
```

`invoke` returns `resultType: "complete"`, optional `instructions` (a nonempty
string when present), and `content` (MCP content blocks). Instructions become
the activation's current instruction contribution; content reports this
invocation's outcome. Omitting instructions retains the existing contribution,
if any. The host records returned instructions for context rebuilding instead
of invoking the server again when rebuilding context.

Invocation may have effects. It is not a resource read or client-side prompt
expansion. Every invocation carries a host-issued stable `invocationId` in
session metadata. The provider deduplicates retries and rejects reuse with
different semantic arguments. A fresh invocation of an already active skill
calls MCP again with a new invocation ID, using the same activation. A missing
revision or withdrawn registration fails explicitly; neither endpoint silently
substitutes another skill.

This extension permits `frances/hostInputRequired` on `frances/skills/invoke`,
with the base protocol's continuation and receipt rules. A completed invocation
may carry the existing context transition metadata only when its provider is
the selected controller. Invoking a regular sub-skill neither grants nor
revokes that provider's existing controller authority.

Additional material can be supplied through ordinary MCP resources referenced
by the instructions. References retain provider provenance and do not authorize
arbitrary host filesystem reads. Local skill files can be exposed by an adapter;
this extension does not require a particular on-disk package format.

## User syntax and disambiguation

Use Codex-style `$name` mentions, such as `$frances` or `$review`. Codex documents
explicit `$` invocation in its [skills guide](https://learn.chatgpt.com/docs/build-skills).
The qualification and picker rules below are this extension's host behavior.

`$provider/name` selects a skill from a particular configured MCP provider. For
example, `$frances/frances` invokes the `frances` skill on the provider whose host
alias is `frances`. Provider aliases are unique within the host session and use
the same character set as skill names. They are host configuration, not a name
claimed by the remote server. A second configuration of the same server gets a
different alias.

For an unqualified user invocation, resolve against all currently available
skills, including sub-skills registered by the active primary:

| Matches | Host behavior |
| --- | --- |
| None | Report that the skill is unavailable; make no MCP invocation |
| One | Resolve that skill and proceed through normal invocation checks |
| Multiple | Show a native picker and wait for the user's selection |

Each picker option shows the qualified name, description, and primary/regular
mode. Sub-skills also show their owning primary. The host MUST NOT silently
prefer the active primary's provider, the first match, or a previous selection.
For example, `$review` may offer `$frances/review` and `$engineering/review`.
Cancelling invokes nothing and does not silently send the unresolved command
as an ordinary model prompt. The draft input remains available for editing.

Resolution happens before invoking a candidate or changing activation state.
The picker is host UI; no candidate server controls it, and it does not require
the `frances/ui` extension. Preserve the user's accompanying input while they
choose. Multiple mentions are resolved before any of them are invoked; execution
is ordered and stops on failure, without pretending prior effects can roll back.

Bind the selection to the exact provider, skill revision, and registration
identity when applicable. Revalidate availability before dispatch. A skill
withdrawn or replaced while the picker is open cannot be silently substituted.
A qualified mention skips the picker, but not availability or authority checks.

Model-facing discovery includes structured identities. Model-requested
invocations use those identities rather than ambiguous short names. An ambiguous
model request receives an error listing the candidates; the host does not invent
a user selection. Mention parsing applies to intentional invocations in user
input, not quoted examples, code, tool results, or provider content.

## Dynamic sub-skills

The active primary's MCP provider can register and deregister regular sub-skills
for that primary activation. These are additional callable entry points, not
automatically loaded instructions or automatically executed actions. Existing
regular skills from any provider remain available alongside them.

Propose two host operations, carried through the base protocol's
`frances/hostInputRequired` continuation:

- `frances/skills/register`: `primaryActivationId` and `skill` (a regular skill
  descriptor). Returns a host-issued `registrationId` identifying this exact
  registration, which the host stores with its provider provenance.
- `frances/skills/unregister`: `primaryActivationId` and `registrationId`.
  Removes that registration and deactivates it if active.

These operations extend the allowed `hostRequests` methods when `frances/skills`
is negotiated. They can be requested from a skill invocation, hook, tool, or UI
response using the existing continuation mechanism; they are not independent
server-to-host RPCs. Registration supplies routing and discovery metadata.
The provider implements the entry point reached by `frances/skills/invoke`.

The host accepts mutations only from the primary's provider, bound to the
current primary activation when the originating call was issued. Supplying an
activation ID does not grant authority. A primary cannot register skills for a
different provider, modify another provider's catalog, or register another
primary as its child. Regular skills cannot create a recursive ownership tree.

Names must be unique within a provider's currently available catalog, including
its static skills. Registration rejects collisions instead of shadowing a
static or dynamic skill. Cross-provider collisions are allowed and use the
picker above. Re-registering a withdrawn name creates a new registration ID;
an old reference cannot invoke the new registration accidentally.

The host persists accepted registrations and continuation receipts before
returning them. Retrying an operation returns its recorded result without
duplicating a registration. Unregistering an already removed registration is
idempotent for its original owner. Updates to a registered skill require
withdrawal and a new registration, rather than changing an active definition.

For example, `$frances` can register `$clarify` while gathering requirements,
then withdraw it and register `$review` when review becomes useful. Invoking
`$frances/review` calls that server with the selected registration ID and the
user's input. What those calls do is entirely provider-defined.

After deregistration, new invocations fail and queued invocations revalidate
before dispatch. An already dispatched call may have performed effects; finish
or reconcile it rather than claiming deregistration undid its work. Removal
revokes further activation authority and takes effect before the next model
request. Instructions already in history follow the deactivation rules below.

Ending or switching the primary removes all its registrations as well as its
active regular skills, even if the provider cannot be reached. Context
replacement, interruption, and restart preserve registrations. Discovery and
the model's skill inventory reflect accepted changes before the next model
request; a stale picker or model reference remains subject to dispatch checks.

## Activation and lifetime

The host owns activation. Both direct user invocation and model-requested use
go through the same checks. A model-facing activation tool is a host adapter,
not permission for a provider to send independent server-to-host requests.
The host records the source of the request; the model cannot claim user origin.
For Frances, activating the primary `frances` skill requires explicit user choice.

Before invoking a skill, the host resolves its identity, checks required
capabilities and policy, and reaches a safe model boundary. It settles unrelated
outstanding calls before changing the active set. Skill invocation effects need
the same host authorization and applicable hook coverage as tool execution;
mention syntax is not a permission bypass.

Each activation has a host-generated `activationId` and pinned descriptor. The
host persists a pending activation before issuing its first MCP invocation.
A pending primary reserves the single slot and can register sub-skills during
that invocation. Competing primary requests serialize at the host. Once the
invocation completes, the host records its result and instruction contribution
before the next model request. No model execution proceeds during setup.

An uncertain invocation is recovered using the same invocation ID before
continuing. A setup failure is reported and pauses the pending activation;
the user can leave it, removing any registrations already accepted. The host
does not claim that external effects were rolled back. Pre-dispatch validation
failures leave the active set unchanged. Repeated invocations do not allocate
another activation or silently change its pinned revision.

Proposed lifetime rules:

- A primary stays active across turns, interruptions, and context replacements.
  An assistant final response or a request for user input does not release it.
- Regular skills activated while a primary is active belong to that primary
  activation. They remain active until explicitly deactivated or the primary
  ends. They do not form a nested call stack and may finish in any order.
- Regular skills activated without a primary belong to the host session. When
  a primary starts, existing regular activations join it, so they receive the
  same lifetime rules as regular skills activated afterward.
- Ending a primary also deactivates its regular skills. Provider application
  state and conversation history remain; ending an activation is not deletion.
- Activating a different primary while one is active fails with
  `primarySkillActive`, identifying the current primary to the host. There is
  no implicit preemption, suspension stack, or queue of primary skills.

The host provides explicit leave and switch actions. Switching ends the current
primary and its regular skills and reserves the new primary as one accepted
host state change, after validating the new descriptor. The new primary's MCP
invocation then runs under the setup rules above. The user can leave even
if the provider is unavailable. Switching never implies approval of unfinished
work or rolls back external effects.

An agent can request completion of the primary through a host operation. The
host checks its configured completion policy before accepting it; prose such as
"done" is insufficient. A regular skill cannot request release on behalf of
the primary. The precise operation surface and how provider completion hooks
participate need a wire-design decision before implementation.

## Context and tool selection

The host retains returned instructions and rebuilds each activation's contribution
after context replacement. Ordering is deterministic: host instructions, primary
skill instructions, then regular skills in activation order. User input remains
user input. Ordering among regular skills does not confer extra authority.

Replacing context does not deactivate skills, invoke them again, or
discard the primary slot. The provider remains responsible for its application
state, such as a plan or review history. That state is separate from the skill
descriptor, returned instructions, and host activation records.

Activation can add instructions at a safe boundary without changing the tool
set. It cannot add tools to an existing context. The base protocol's fixed tool
selection still applies: changing selected tools requires context replacement.
If a skill cannot operate with the available tools, setup must arrange an
authorized replacement or report the missing requirement before model execution.

Dynamic sub-skills change the skill inventory, not the MCP tool inventory. A
host can provide a fixed generic skill-invocation tool whose arguments identify
the skill. Registration must not create one new model-facing tool per sub-skill
or change that tool's schema. Invocation still checks the live registry, host
permissions, and applicable primary policies. Required editing or shell tools
do not become available merely because a skill mentions or wraps them.

Deactivation removes future instruction contributions but cannot make a model
forget instructions already present in conversation history. It revokes any
authority tied to the activation immediately. The host communicates the change
to the model; a clean removal of historical instructions requires replacement.
Any proposed activation-driven replacement must commit consistently with the
activation state, using the base transition receipt and recovery rules.

## Relationship to hooks and controller authority

The base protocol already allows one host-selected controller. A primary skill
and a controller are different concepts: the skill is active instruction and
activation state; the controller is a provider allowed to propose context
transitions. A primary containing only instructions needs no controller.

Advertising a primary skill does not grant controller authority. A host may
bind selection of its provider as controller to an accepted primary activation,
subject to capability checks and policy. That binding lasts only for that
activation. A regular activation never changes the selected controller.

A host must reject an incompatible existing controller binding before accepting
a primary that needs controller authority. Ending a primary revokes a binding
owned by that activation. Outstanding transitions must be reconciled before a
different binding starts; delayed results from an ended activation cannot gain
authority merely because the same provider is selected again.

Providers advertise their published hook URIs up-front under the base protocol.
Subscriptions can change during a skill activation or be requested at boot while
their publishers are still unavailable. Full catalog updates report established
subscriptions and pending intent. Skills use these existing subscription operations;
sub-skill registration does not itself advertise a new hook type.

The subscription key remains `(MCP, hook URI)`, not a skill or handler ID. A
provider coordinates its own handlers and any shared subscription needs across
skills. Ending one skill must not unsubscribe a hook another active skill needs
or disable required host policy. Every handler dispatched within one protocol
invocation belongs to the same evaluation frame; local handler dispatch cannot
reset the host's recursion stack.

Notifying providers of skill activation changes and fencing results by activation
identity still require explicit wire contracts. Skill-scoped behavior must not
silently turn off mandatory policy.

## Persistence and recovery

The host persists the active set, pending activations, primary ownership,
regular-skill membership, dynamic registrations, pinned descriptors, returned
instructions, controller bindings, and invocation receipts. It restores
them before model execution and reconciles pending context transitions under
the base protocol's recovery rules. A disconnect, restart, timeout, or interrupt
does not silently release the primary.

An unavailable required provider pauses affected execution and reports the
problem. The host can still let the user leave the skill. A lost descriptor or
receipt is a recovery error, not a reason to guess whether activation succeeded.
Host activation state is authoritative; a provider cannot acquire the primary
slot by persisting its own claim.

## Example

1. The user invokes `$frances/frances` with a task. The host reserves the primary
   slot and calls its MCP provider. The provider returns initial instructions
   and registers the sub-skills useful at this point.
2. Its instructions and hooks lead the agent to interview the user and maintain
   a plan in provider-owned state. A regular engineering skill can also be used.
3. As work progresses, the provider withdraws `$clarify` and registers `$review`.
4. The user invokes `$review`. If multiple providers offer it, the host opens a
   picker. Selecting `$frances/review` calls the Frances MCP provider; cancelling
   invokes nothing. Typing `$frances/review` directly skips the picker.
5. The provider performs its review behavior and may propose a fresh context
   if it has controller authority. Active skills and registrations survive.
6. Another primary is requested. The host reports the conflict; `$frances`
   remains active unless the user explicitly switches.
7. Completion is accepted or the user leaves. The primary, its regular skills,
   and its registrations end. The provider's saved plan remains available.

None of these steps requires the host to know what a plan or review means.

## Decisions to settle during wire design

- Confirm the names `primary` and `regular` and the proposed lifetime rules,
  especially adopting existing regular skills when a primary starts.
- Specify host activation, completion, leave, and switch operations, their
  receipt keys, errors, and revision checks. Keep user authority distinguishable
  from model requests.
- Complete invocation and registration schemas, including activation metadata,
  authorization descriptions and hook envelopes for skill invocation, and
  recovery of partially completed setup and registration changes.
- Define descriptor fields for required tools, hooks, and controller authority;
  align tool requirements with the base protocol's pending inventory design.
- Define activation metadata, provider notification and reconciliation, and
  stale-result fencing without introducing a second session lifecycle.
- Specify how acceptance of an activation and any required context replacement
  share one durable decision, including recovery and explicit user departure.
- Decide whether regular skills need a shorter automatic lifetime. This proposal
  uses explicit deactivation and primary membership instead of guessing when an
  instruction has finished applying.

## Review scenarios

An implementation should demonstrate concurrent primary requests across two
providers accepting only one; multiple regular skills under that primary;
regular skills without a primary; activation retries without duplicate injection;
context replacement and restart preserving the active set; explicit switching
without an intermediate model request; unavailable providers still allowing user
departure; stale results failing after release and reactivation; and attempts to
use skill activation to bypass fixed tool selection or host permissions failing.

Also demonstrate sub-skill registration without invocation; an invocation reaching
the owning MCP server with input and stable identity; retries without duplicate
effects; repeated deliberate invocations executing again; deregistration blocking
stale calls without undoing completed effects; registration cleanup on primary
exit; ambiguous mentions opening a picker before any call; picker cancellation;
qualified invocation skipping the picker; and withdrawal or re-registration
while a picker is open never invoking a replacement implicitly.
