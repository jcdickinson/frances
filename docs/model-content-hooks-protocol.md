# Model Content + Hooks Protocol

Status: draft protocol, not implemented. Extension version: 1. The prerequisite
native Rust harness is implemented without MCP or the planning workflow.

This document specifies a family of MCP extensions for hosts that run coding
agents. The working name is Model Content + Hooks Protocol. All extension
capabilities and RPC methods use the `frances/` namespace, but their contracts
must not depend on Frances, Rust, a particular model provider, or a particular
plan format.

The reference MCP baseline is [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25).
Existing MCP methods retain their meanings. Everything under `frances/` below
is proposed here, not part of the MCP standard. MUST, SHOULD, and MAY describe
requirements of this draft. Wire examples are illustrative instances of those
requirements, not a complete machine-readable schema.

## Naming conventions

Extension fields use camelCase, following MCP's naming conventions, for example
`canonicalUri` and `contextId`. Hook event names use slash-separated paths, such
as `tool/use/before` and `tool/use/batch/after`; `stop` and `interrupt` are single
segments. RPC methods also use slash-separated namespaces, such as
`frances/authorization/describe`. Existing MCP names, URI schemes such as
`workspace-file`, and tool-provider-defined names are preserved. Hook event names
are this protocol's convention, inspired by MCP method naming; references to other
hosts do not import their event spellings.

## Purpose

A host owns the agent loop, model connection, tool execution, permissions,
interruptions, and user interface. Servers supply tools, resources, and optional
hooks that influence that loop. A selected controller can replace the model's
context and continue execution without restarting the application or requiring
the user to send another message.

The motivating workflow is Frances's [recorded main workflow](arch/main-workflow.md):
interview the user, maintain a structured plan, execute one step, review its
completion, save the result, and start the next step with fresh context.
Planning is a server application of the protocol, not a protocol primitive.

Frances will provide that application: port the existing main workflow into a
supplied MCP server after replacing the JS runtime with an ordinary Rust harness
without MCP support. Preserve its planning,
step validation, review, summarization, and context progression behavior, with
the explicit plan approval and fixed context tool sets specified here and in the
UI extension. The port is a later deliverable, not a prerequisite for removing
JS; other protocol hosts are not required to ship this particular workflow.

Frances should implement its ordinary agent loop in Rust and remove the embedded
JS workflow runtime. This document specifies the external contract; it does not
require another host to make the same implementation choice. Frontend JavaScript
is unrelated to removing the workflow runtime.

## Roles and ownership

| Role | Responsibility |
| --- | --- |
| Host | Runs the agent, enforces permissions, persists conversation history and accepted context transitions |
| Tool provider | Implements MCP tools and describes their authorization requirements |
| Hook provider | Receives host events and returns decisions or context |
| Controller | Hook provider selected by the host to control context transitions |

One MCP server can fill all three server roles over the same connection. A host
can connect multiple tool and hook providers. At most one controller is active
for a host session. Supporting a capability does not grant controller authority;
selection is host configuration.

The server owns its plan, phase, step history, and persistence. The host MUST NOT
require the server to serialize that application state into a host-owned store.
The host may retain protocol receipts and transcript data without understanding
the plan. Servers may expose the plan through ordinary MCP resources.

## Extension negotiation

The companion [UI specification](model-content-hooks-ui.md) defines `frances/ui`
for native choices, forms, plan approval, progress, and artifact views.

Capabilities are exchanged during MCP initialization. The
[MCP lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
provides `experimental` fields on both sides for non-standard features.

Each side advertises supported positive integer versions independently:

```json
{
  "capabilities": {
    "experimental": {
      "frances/authorization": { "versions": [1] },
      "frances/hooks": { "versions": [1] },
      "frances/context": { "versions": [1] }
    }
  }
}
```

For each extension, use the highest version present in both advertisements.
No intersection means that extension is unavailable. A server MUST NOT infer
support from the client's product name. A host MUST NOT silently substitute
ordinary chat for a configured controller whose required extensions are absent.

| Extension | Contract |
| --- | --- |
| `frances/authorization` | Describe a tool invocation without executing it |
| `frances/hooks` | Discover subscriptions and deliver lifecycle events with responses |
| `frances/context` | Propose, acknowledge, and recover context transitions |
| `frances/ui` | Publish semantic UI and receive user decisions; defined in the companion spec |

`frances/context` requires `frances/hooks`. Authorization
descriptions can be implemented independently. A hook can operate without
authorization descriptions, but MUST then treat effects it cannot determine as
unknown. Standard tools, resources, and sampling capabilities remain separate.

## Sessions and persistence

Two protocol identities have different meanings:

| Identity | Meaning |
| --- | --- |
| MCP session ID | Server-issued protocol session, durably retained when server state depends on it |
| Context ID | Host-issued identifier for one model conversation within that session |

Ordinary MCP initialization establishes the server session; there is no separate
attachment handshake. The host's internal session ID does not cross the wire.
The host maps its user session to the server's MCP session locally.

For Streamable HTTP, the host persists `MCP-Session-Id` with the server connection
configuration and authentication identity and sends it on subsequent requests.
Servers providing `frances/context` MUST persist the MCP session ID and any
application state needed to resume it for the lifetime of that session, including
across process restarts. There is no idle expiry: a server that needs the state
retains it indefinitely until explicit session deletion. An application exit or
transport disconnect is not a request to delete the session.

This is a stronger lifetime contract than base MCP's optional HTTP session
management. See [HTTP session management](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#session-management).
The host reconnects using the saved ID. An unexpected unknown session is an error,
not permission to silently create an empty plan. Deliberately creating a new
session remains possible, but is not recovery of the old one. The protocol does
not require plan snapshots in the host or an expired-session recovery handshake.

The server MUST scope access to the authenticated client; possession of a session
ID alone is not authorization. The host MUST allocate a distinct controller MCP
session for each independent user session. Concurrent writers to the same
controller session are not supported and MUST NOT have their events mixed.

The host persists the session ID, context identity, revision, and transition
receipts. The server chooses its database, subject to the lifetime contract above.
Neither the header nor these extensions promise that an ordinary tool call
executes exactly once after a network failure.

The durable session resumption mechanism for stdio remains an open design question;
`MCP-Session-Id` is an HTTP mechanism and MUST NOT be presented as a stdio header.

## Authorization descriptions

### Tool declarations: static or per-call

MCP tool definitions provide `_meta` for extension metadata and `inputSchema`
for their argument schema. This extension uses `_meta["frances/authorization"]`,
not a new top-level `extensions` property. See the
[MCP Tool schema](https://modelcontextprotocol.io/specification/2025-11-25/schema#tool)
and [metadata key rules](https://modelcontextprotocol.io/specification/2025-11-25/basic#general-fields).

The value is either a static authorization description or the literal string
`"authorize"`. A static description applies to every valid invocation of that
tool, including all argument values and relevant server states. It may describe
a conservative set of targets rather than only the files a particular call reads.
The host uses it directly without a description RPC. For example:

```json
{
  "name": "read_agent_instructions",
  "description": "Read AGENTS.md files in the workspace.",
  "inputSchema": {
    "type": "object",
    "properties": { "path": { "type": "string" } },
    "required": ["path"]
  },
  "_meta": {
    "frances/authorization": {
      "effects": "known",
      "tags": ["read", "file"],
      "description": "Reads AGENTS.md files in the workspace.",
      "targets": [
        {
          "uriPattern": "workspace-file:///**/AGENTS.md",
          "tags": ["read", "file"]
        }
      ]
    }
  }
}
```

This tool MUST reject paths outside the declared set. A general-purpose file
reader cannot use this declaration merely because most calls read AGENTS.md.
Static descriptions contain no argument interpolation or executable expressions.

A tool whose description depends on its arguments explicitly requests lookup:

```json
{
  "name": "read_file",
  "inputSchema": {
    "type": "object",
    "properties": { "path": { "type": "string" } },
    "required": ["path"]
  },
  "_meta": { "frances/authorization": "authorize" }
}
```

For `"authorize"`, the host calls `frances/authorization/describe` before executing
each invocation. The marker selects that fixed RPC method; it is not the name of
another tool. Session-level capability support alone MUST NOT trigger lookups for
every tool. Missing metadata means unknown effects. Invalid metadata or a failed
advertised lookup is an error, not a fallback to a more permissive description.
Without negotiated extension support, this metadata grants no permission.

The host invalidates cached declarations when the tool list changes or the server
connection is reinitialized. This refreshes the inventory for future contexts;
it MUST NOT change the active context's tool definitions. If an active tool's
definition or authorization contract has changed, the host pauses affected calls
until an explicit context replacement selects the current definition. Static and
dynamic descriptions use the same result
shape: `effects`, `tags`, `targets`, and optional human-readable `description`.
The host attaches the operation URI and provider identity before invoking policy
hooks; the server does not choose its own authority. Authorization descriptions
are host-facing metadata and do not automatically enter the model's prompt.

### `frances/authorization/describe` — host to tool provider

The request is identical to a regular `tools/call` request except for its RPC
method. In particular, `params` is MCP's `CallToolRequestParams`, not a new shape
containing a nested `toolCall`, session ID, or authorization-specific arguments.

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "frances/authorization/describe",
  "params": {
    "name": "read_file",
    "arguments": { "path": "README.md" }
  }
}
```

The corresponding execution request has the same parameters:

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "read_file",
    "arguments": { "path": "README.md" }
  }
}
```

The requests have independent JSON-RPC IDs. Optional MCP request metadata is
accepted under the same parameter schema. A description MUST NOT execute the
tool, mutate the requested resources, or advance workflow state. It may inspect
metadata needed to resolve targets. Unknown tools and invalid parameters produce
ordinary JSON-RPC errors.

The baseline schema also admits task metadata. Negotiating task support for
`tools/call` does not authorize task augmentation of this extension method. A
version 1 receiver MUST reject task-augmented description requests explicitly;
the eventual task-enabled execution is still subject to completed authorization.
This is a method capability restriction, not a different parameter schema.
See [MCP CallToolRequestParams](https://modelcontextprotocol.io/specification/2025-11-25/schema#calltoolrequestparams).

Example result:

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "result": {
    "effects": "known",
    "tags": ["read", "file"],
    "description": "Reads the workspace README.",
    "targets": [
      {
        "uri": "workspace-file:///README.md",
        "canonicalUri": "workspace-file:///README.md",
        "tags": ["read", "file"]
      }
    ]
  }
}
```

`effects` is `known` or `unknown`. A known description asserts that its tags and
target selectors conservatively cover the invocation's effects; it need not enumerate
every file in a search. An unknown description may include known targets and tags,
but their presence MUST NOT be interpreted as an exhaustive description.

### Identifiers, effects, and targets

Operation URIs identify operations, not a particular set of arguments:

- `platform://filesystem/read_file`
- `platform://filesystem/search`
- `mcp://ferrisfetch/docs`

Tags and targets are structured fields, not URI query parameters. Search text,
patch contents, and other tool arguments stay in the original invocation.

The host constructs the operation URI from its tool inventory and binds an MCP
authority to the configured provider instance. A server cannot impersonate another
provider or a platform operation.
The hook receives host-established provenance separately from the provider's
description. Provider assertions are not automatically trusted security facts.

Core tags are `read`, `write`, `search`, `file`, `directory`, `execute`, and
`network`. `write` includes creation, modification, rename, and deletion. Tags
describe effects that may occur, not only the tool's intended purpose. A mixed
operation includes all applicable tags. Additional tags MUST use a namespace.
Unrecognized tags MUST NOT grant permission.

Each target contains `tags` and exactly one selector: `uri` for a literal resource
or `uriPattern` for a glob over resource paths. `targets` is always an array.
Concrete filesystem targets additionally carry `canonicalUri` when resolved.
There are no `resolution` or `scope` fields: the URI scheme establishes the path
base, and a pattern explicitly describes a set of resources.

- `file:///home/user/project/README.md` identifies an absolute filesystem path in
  the tool's execution environment.
- `workspace-file:///README.md` identifies a path relative to the workspace root
  bound to this server session. The leading slash does not mean the filesystem
  root. This is a proposed scheme specific to this protocol.
- `workspace-file:///**/AGENTS.md` in `uriPattern` selects matching instruction
  files at any depth, including directly at the workspace root.

For both `file` and `workspace-file`, the authority MUST be empty. A host, user
information, or port is invalid, including `file://localhost/...` and
`file://workspace/...`. Use the explicit triple-slash forms above. These selectors
identify files in the provider's execution environment, never a remote host named
in the URI. This restricts the broader
[file URI scheme](https://www.rfc-editor.org/rfc/rfc8089.html).

A workspace binding must resolve to one unambiguous canonical root; multi-root
hosts must establish that binding explicitly. It remains fixed for a session.
Changing roots requires a new binding/session and new authorization, not reusing
a decision for the same URI at a different root.

### Requested and canonical paths

`uri` identifies the path as requested, encoded as a URI. It MUST preserve the
requested symlink path rather than replace it with the resolved destination.
Workspace-relative requests use `workspace-file`; absolute requests use `file`,
even when that absolute path is inside the workspace. The original tool arguments
remain available separately.

`canonicalUri` identifies the fully resolved destination, with symlinks resolved.
Its scheme is selected from the resolved location, independently of `uri`:

- If the destination is the canonical workspace root or lies inside it,
  `canonicalUri` MUST use `workspace-file:///` with the path relative to that root.
- Otherwise it MUST use a hostless absolute `file:///` URI.
- Containment uses filesystem path components, not a string prefix. Both the
  workspace root and destination must be canonicalized in the same environment.

A symlink within the workspace may resolve outside it. This is valid and MUST NOT
be rejected merely because of the symlink or the change of URI scheme. For example:

```json
{
  "uri": "workspace-file:///AGENTS.md",
  "canonicalUri": "file:///home/user/shared/AGENTS.md",
  "tags": ["read", "file"]
}
```

An absolute request whose resolved destination is inside the workspace instead
uses a workspace-relative canonical URI:

```json
{
  "uri": "file:///home/user/project/AGENTS.md",
  "canonicalUri": "workspace-file:///instructions/AGENTS.md",
  "tags": ["read", "file"]
}
```

Policy can match the requested identity, the canonical destination, or both.
A rule allowing requested `workspace-file:///**/AGENTS.md` paths can therefore
allow a user's linked instruction file without an extra prompt. A separate rule
restricting resolved destinations evaluates `canonicalUri`. Neither check is
silently substituted for the other.

Per-call descriptions MUST include `canonicalUri` for concrete filesystem targets
they successfully resolve. Static declarations may omit it; hosts MUST NOT assume
that an omitted canonical URI equals the requested URI. A static value is valid
only if it is invariant for every invocation covered by the declaration. A glob
has no single canonical destination and MUST NOT carry `canonicalUri`; any
canonical checks must apply to its concrete matches. Providers needing per-call
resolution can declare `"authorize"` instead of static metadata.

If a target cannot yet be canonicalized, for example a new file, omit
`canonicalUri` rather than label an unresolved path canonical. A policy requiring
a canonical destination cannot be satisfied by its absence. Detailed creation
semantics remain a draft question. Canonicalization is an observation, not a lock:
the executor must enforce any approved destination boundaries at execution time.

### Path patterns

These are glob patterns, not regular expressions. Minimatch is one implementation
of glob matching, not the wire specification. Version 1 defines a small portable
subset rather than depending on a JavaScript package's defaults:

- `*` matches zero or more characters within one path segment.
- `?` matches one character within one segment.
- `**` as a complete segment matches zero or more complete path segments.
- Matching covers the entire path and includes dot-prefixed names. It is case
  sensitive; a host must reject or conservatively handle a policy it cannot enforce
  on a filesystem with different name-equivalence rules.
- Braces, character classes, extglobs, leading negation, and backslash escapes are
  not supported. Unsupported pattern syntax is an error.
- Scheme and authority are literal, not globbed. Query strings and fragments are
  not accepted in filesystem target selectors.
- Percent-encoded characters are literals, including encoded `*` and `?`. Decode
  once per segment; encoded separators, NUL, and `.`/`..` segments are rejected.
  Separators are `/` on every platform.

Use `uri` for literal paths containing glob characters. Use a directory's `uri`
to describe access to the directory itself, and `uriPattern` ending in `/**` to
describe it and its descendants. Matching is over parsed paths, not raw string
prefixes. Implementations must not treat overlap between two patterns as proof
that one permitted target set contains the other; uncertain containment requires
more precise authorization or user approval under host policy.

Operation tags cover the whole call. Target tags associate effects with individual
resources, for example the read source and write destination of a copy.
The host must retain provider provenance when comparing filesystem targets.
A hostless URI from a server reached over HTTP names a file in that provider's
execution environment; it does not instruct the host to read its own filesystem.

For a search, the operation is `platform://filesystem/search`, its tags are
`read`, `search`, `directory`, and `file`, and its target can be
`{ "uriPattern": "workspace-file:///**", "tags": ["read", "directory", "file"] }`.
Its query remains in the arguments. This lets a hook permit
filesystem reads without depending on whether the native tool is Read or Grep.

A generic filesystem-read rule requires a known description, the filesystem
operation authority, a `read` tag, only permitted effect tags, and permitted
targets. Merely containing `read` is insufficient. Descriptions do not eliminate
filesystem races: the executor must enforce any promised path boundaries when
opening files. A shell command with uncertain effects remains unknown unless the
execution environment actually enforces a narrower set of effects.

Native tools use the same description contract, produced by a host adapter.
An MCP tool without this capability remains usable under host policy, but is
described as unknown rather than assumed safe.

## Lifecycle hooks

### Discovery

`frances/hooks/list` is a host-to-server request with empty parameters. Its result
contains `hooks`, an array of `{ id, events }` subscriptions. Version 1 subscriptions
remain fixed for the MCP session. Hosts choose which subscriptions to activate and
whether a subscription is required. A server cannot make itself mandatory.

| Event | Delivery point | Permitted response |
| --- | --- | --- |
| `session/start` | After MCP initialization or reconnection and pending-transition reconciliation, before model execution | Context, controller transition |
| `context/start` | After replacement, before the next model request | Context only |
| `prompt/submit` | Before adding new user input | Accept or deny with reason, context |
| `tool/use/before` | After argument validation and description, before execution | Authorization decision, context |
| `tool/permission` | When host policy requires approval | Authorization decision |
| `tool/use/after` | After one call settles, including failures or denials | Context, controller transition |
| `tool/use/batch/after` | After all calls in a model response settle | Context, controller transition |
| `stop` | Model would finish without further tool calls | Wait, continue, or controller transition |
| `interrupt` | User interrupts execution | Observation only |
| `session/end` | Host detaches from the session | Observation only |

The names and tool decision points draw on
[Claude Code hooks](https://code.claude.com/docs/en/hooks#pretooluse-decision-control)
and [Codex hooks](https://learn.chatgpt.com/docs/hooks). This draft defines its own
coverage and semantics rather than inheriting either host's implementation gaps.
`session/end` does not delete server state.

### `frances/hooks/invoke` — host to hook provider

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "frances/hooks/invoke",
  "params": {
    "hookId": "planning-policy",
    "eventId": "event-18",
    "contextId": "context-7",
    "revision": 12,
    "event": {
      "type": "tool/use/before",
      "invocationId": "call-9",
      "provider": { "kind": "platform" },
      "call": {
        "name": "read_file",
        "arguments": { "path": "README.md" }
      },
      "authorization": {
        "operation": "platform://filesystem/read_file",
        "effects": "known",
        "tags": ["read", "file"],
        "targets": [
          {
            "uri": "workspace-file:///README.md",
            "canonicalUri": "workspace-file:///README.md",
            "tags": ["read", "file"]
          }
        ]
      }
    }
  }
}
```

An MCP provider is represented as `{ "kind": "mcp", "id": "ferrisfetch" }`.
The host supplies this identity. `call` preserves MCP call parameters, including
metadata; native tools are adapted to that shape. This hook envelope does not
change the separate authorization request's parameter shape.

Other event payloads carry the data needed at their delivery point:

- `session/start`: reason `new` or `resume`.
- `context/start`: applied transition ID and reason.
- `prompt/submit`: submitted MCP content blocks.
- `tool/permission`: invocation, authorization description, and host approval reason.
- `tool/use/after`: invocation ID, final call, and tagged outcome: result, denied,
  cancelled, or execution error. A result carries an MCP `CallToolResult`.
- `tool/use/batch/after`: settled invocation IDs in model order and assistant content.
- `stop`: final assistant content and the count of automatic continuations.
- `interrupt` and `session/end`: reason.

The complete per-event JSON Schemas are a follow-up to review of these contracts.
Hosts MUST reject response actions not permitted for the triggering event.

### Decisions and ordering

Authorization responses contain `decision` with one of these shapes:

```json
{ "decision": { "type": "deny", "reason": "File writes are disabled during planning." } }
```

Other variants are `abstain`, `allow`, and `ask` (with a reason). Denial is returned
to the model as a tool failure explaining what was blocked. `abstain` means the
hook supplies no decision. For multiple hooks, `deny` wins over `ask`, which wins
over `allow`; abstentions have no effect.

`tool/use/before` allowance permits the call through workflow policy; it does not
override host permissions or sandboxing. `tool/permission` allowance can answer
an approval prompt only if the host explicitly delegated that authority to the
provider. Otherwise the normal approval path remains. `ask` never implicitly
approves an operation when an interactive user is unavailable.

Version 1 does not allow hooks to rewrite arguments. This avoids approval of one
invocation followed by execution of another. If rewriting is added, the changed
call must be validated, described, and authorized again before execution.

All model-requested platform and MCP tool calls MUST pass activated pre-execution
hooks, including calls made through a batch or code-execution wrapper. If a host
cannot intercept a tool, it MUST disclose that gap and MUST NOT offer that tool
under a controller policy requiring full coverage. File attachments and other
non-tool context inputs are not implicitly covered by a tool hook.

Required hook failures, invalid results, and timeouts pause the affected operation
and report an error. They MUST NOT be interpreted as allowance. Optional observer
failures are reported but need not block. Post-execution denial cannot undo a write.

Within a controller session, the host delivers stateful events serially in a
stable order. Tools may execute concurrently, but results and hooks are delivered
in model call order. Requests generated by the protocol itself, such as hook
invocations and authorization descriptions, MUST NOT recursively trigger tool
hooks. Hosts must still service MCP sampling requests while awaiting a hook.

`stop` responses choose `wait` or `continue`; `continue` includes model-facing
content explaining the remaining work. Hosts bound automatic continuations and
expose exhaustion to the user. An interrupt always suppresses automatic restart.

## Model content

Hook responses may include `content`, using MCP content blocks. The host converts
these to appropriate model inputs: text is extracted, and supported media is
mapped to the provider's native representation. Control metadata, JSON-RPC
envelopes, session IDs, and authorization descriptions MUST NOT be dumped into
model context by default.

Resource links are references, not an instruction to fetch arbitrary content.
When a controller needs exact seed content, it should supply text or embedded
resources. Hosts MUST report unsupported required content rather than silently
omit it from a replacement context. Host policy determines instruction priority;
server text cannot replace mandatory host instructions.

## Context control

A negotiated, selected controller can propose a transition in a permitted hook
response. It can also include the same object in an ordinary tool result under
`_meta["frances/context"]`. Keeping the transition with the tool result ensures
that `plan_exit` does not race a separate clear notification.
When `frances/ui` is negotiated, the selected controller may also return this
metadata in a `frances/ui/respond` response, for example after explicit plan
approval. The same authority, revision, and acknowledgment rules apply.

```json
{
  "content": [{ "type": "text", "text": "Planning complete." }],
  "_meta": {
    "frances/context": {
      "transitionId": "transition-4",
      "expectedRevision": 12,
      "action": {
        "type": "replace",
        "reason": "Begin the first execution step",
        "instructions": "Execute only the active step. Submit proof when finished.",
        "content": [{ "type": "text", "text": "# Agreed plan\n\n..." }],
        "next": "run"
      }
    }
  }
}
```

`next` is `run` or `wait`. Replacement creates a new context ID with no prior
model conversation. Host instructions and environment are rebuilt; controller
instructions and seed content are then installed. Workspace files, UI transcript,
durable server state, and host session identity remain. In Frances, the editor's
per-context read cache is recreated while the shared anchor engine remains.

### Fixed tool set per context

The controller selects available tools when creating a context. A replacement
action can include `tools` with these meanings:

| Value | Selection for the new context |
| --- | --- |
| Omitted | Retain the current selection |
| `"default"` | Resolve the host's configured default selection at context creation |
| `[]` | Select no tools |
| Array of operation URIs | Select exactly those tools, subject to host permissions |

`"default"` is not an alias for all installed tools. It explicitly resets the
selection to the host's defaults, so it can restore tools after a restricted
planning context. Once resolved, that selection is frozen like any other;
subsequent changes to host defaults do not affect the active context.

For the initial context, where no current selection exists, omission uses the
host's configured defaults. The controller can replace that initial context
during `session/start`, before the first model call.

The host resolves the selection against its inventory and permissions before
accepting the context, then freezes the model-facing tool names, descriptions,
and schemas for that context. Disabled tools MUST NOT appear in the model's tool
definitions, tool discovery results, or callable surfaces exposed by wrappers.
The same selection applies to every model request within the context.

Changing that selection requires an explicit context replacement with a new
context ID. There is no per-model-call tool filtering hook and no in-place tool
enable/disable operation. `context/start` can add context but cannot change the
selected tools. MCP tool-list changes update the host's inventory for future
contexts; they do not silently alter the active context. If a selected tool
becomes unavailable, the host reports or pauses the affected operation rather
than substituting a different tool set.

For planning, the controller creates a context without editing tools. `plan_exit`
creates a fresh execution context selecting those tools; `plan_begin` creates
another planning context without them. The host does not need to understand
either phase to enforce the selected tool set.

Per-call authorization still applies to selected tools: a file reader can remain
available while a particular path is denied. Such a decision rejects that
invocation without changing the tool list. A guessed or stale invocation of a
tool outside the context's selection MUST be rejected before execution. Tool
selection never overrides host permissions or sandboxing. The portable
inventory-discovery method remains to be specified.

### Safe boundaries and recovery

The host attaches `contextId` and `revision` to calls to the
controller through `_meta["frances/context"]`, without changing the MCP call
schema. A stable `invocationId` in that metadata identifies execution retries.
The authorization description receives the same call parameters.

The server persists a proposed transition before returning it. It MUST retain
unacknowledged proposals and expose them through `frances/context/pending`.
Controller tool calls
that mutate workflow state MUST deduplicate by invocation ID and return the
recorded result for a retry; the same ID with different parameters is an error.
This requirement does not imply idempotence of arbitrary external tools.

`frances/context/pending` is a host-to-controller request with empty parameters.
It returns `{ "transitions": [...] }`, containing unacknowledged proposals in
creation order for the current MCP session. It is read-only and does not create,
attach, or replace a session. After reconnection, the host retrieves and reconciles
these proposals before delivering `session/start` or resuming model execution.

The host MUST:

1. Record the triggering tool result and settle or cancel every outstanding call
   in that model batch. Do not switch context while a call is unaccounted for.
2. Validate controller authority and the expected revision. Conflicting proposals
   cannot be applied in sequence merely because they arrived in sequence.
3. Persist acceptance, the new context identity, and continuation intent before
   issuing another model request.
4. Apply the replacement and deliver `context/start`.
5. Acknowledge the transition with `frances/context/ack`.
6. Run if requested, unless interrupted or awaiting user input.

`frances/context/ack` has parameters `transitionId` and `outcome`. The outcome is
either `{ "type": "applied", "contextId": ..., "revision": ... }` or
`{ "type": "rejected", "reason": ... }`; its result is empty. Duplicate
acknowledgments with the same outcome succeed. Conflicting acknowledgments fail.

Host revisions increase when user input, interruption, or an accepted transition
changes the execution state. Stable event IDs identify redelivery within a
session. Hook providers MUST deduplicate stateful events and preserve their
decisions. After a crash, the host reconciles its receipts with pending server
transitions: already applied transitions are acknowledged again, not applied
again; stale proposals are rejected. Receipt retention must cover the resumable
session lifetime.

This is not a distributed transaction over the plan and host history. The server
must retain enough pending-transition state to reconcile rejection or interrupted
delivery without assuming that the host switched context. A state-changing
controller call whose result is uncertain MUST be recovered before the host
continues ordinary execution.

User input received during a handoff MUST NOT be lost. It is queued for delivery
after reconciliation and invalidates any stale automatic continuation decision.
An interrupted host can record or recover a completed server-side operation, but
MUST NOT resume model execution until the user resumes it.

## Referee and summarizer calls

Use MCP [sampling](https://modelcontextprotocol.io/specification/2025-11-25/client/sampling)
where negotiated, rather than inventing another inference transport. A controller
can request sampling while handling a completion tool or hook. The host remains
responsible for model selection, sampling permissions, and execution budgets.

For isolated review, supply explicit messages and `includeContext: "none"`.
If the referee returns a structured decision through a tool, negotiate
`sampling.tools`. The server validates the verdict and treats missing or malformed
decisions as a failed review, never an implicit approval. Sampling tool requests
do not automatically execute platform tools or bypass their permissions.

The summarizer needs actual evidence. The controller can collect tool outcomes
and assistant content from hooks and retain them server-side. This avoids relying
on a host-local transcript file path that a remote server cannot read. Model
reasoning traces are not required by this protocol.

### Frances sampling model selection

This section defines Frances host configuration and selection policy. It adds no
MCP wire fields and imposes no configuration format on other hosts. Servers use
standard `modelPreferences.hints` and the `costPriority`, `speedPriority`, and
`intelligencePriority` fields. These remain advisory preferences, subject to host
permissions, model availability, request capabilities, and execution budgets.

Each configured model can declare `known_as` names and sampling ratings:

```toml
[models.fast]
model_provider = "example"
id = "example-small"
known_as = ["cheap", "summarizer"]

[models.fast.sampling]
affordability = 0.9
speed = 0.9
intelligence = 0.5

[models.default]
model_provider = "example"
id = "example-large"
known_as = ["referee"]

[models.default.sampling]
affordability = 0.3
speed = 0.5
intelligence = 0.9
```

The provider and model IDs above are illustrative. `known_as` defaults to an
empty list. Names MUST be nonempty, have no leading or trailing whitespace, and
match hints exactly and case-sensitively. They are local routing names, not new
standard MCP model identifiers. For example, `"cheap"` has routing meaning only
when configured; the portable way to prefer cheaper models is `costPriority`.
Multiple model entries MAY share a name, allowing priorities to choose between
them. A unique name targets one configured entry.

Ratings MUST be finite numbers in the inclusive range 0 to 1. Higher means more
affordable, faster, or more capable, respectively. They describe the configured
entry, including its effort and service tier, rather than just the provider's
model ID. These are user-supplied relative ratings, not measured prices or
benchmark guarantees. The `sampling` table is optional; when present, all three
ratings are required. When absent, all three ratings are treated as 0.5 so an
unrated model remains selectable with explicit neutral defaults. Invalid ratings
or names are configuration errors.

For each sampling request, Frances MUST use one configuration snapshot and apply
the following selection rules:

1. Restrict candidates to models allowed and available for this request, with
   support for its required capabilities. A hint never overrides eligibility.
   If no candidate remains, return a sampling error.
2. Evaluate hints in request order. Ignore hints without a name or with an empty
   name. For each hint, first collect exact `known_as` matches. If any exist,
   they win: only those models remain candidates for that hint, regardless of
   other models' configured names, IDs, or scores.
3. If that hint has no `known_as` match, try an exact configured model key match
   (such as `fast`). If none exists, try case-sensitive substring matches against
   provider model IDs. Stop at the first hint with eligible matches; later hints
   do not override it. Thus `known_as` precedence applies within each hint and
   preserves MCP hint ordering.
4. Rank the matched candidates using the score below. If no hint matches, rank
   all eligible candidates instead.
5. Select the highest score. On a tie, prefer the configured `default` entry if
   it is among the tied candidates, then the lexicographically smallest model
   key. Selection MUST NOT depend on map iteration order.

```text
score = costPriority         * affordability
      + speedPriority        * speed
      + intelligencePriority * intelligence
```

Request priorities MUST be finite numbers from 0 to 1; reject invalid values as
invalid sampling parameters. Frances treats omitted priorities as zero. Weights
do not need to sum to one. `costPriority: 1.0` gives the strongest cost preference;
it does not impose a price ceiling. With no positive priorities, scores tie and
the default/tie rule applies within the selected candidates. Without matching
hints or positive priorities, this selects the eligible default model.

For example, with the configuration above, these preferences select `fast` even
though `default` has a higher intelligence rating:

```json
{
  "modelPreferences": {
    "hints": [{ "name": "summarizer" }],
    "intelligencePriority": 1.0
  }
}
```

If two entries share `known_as = ["summarizer"]`, the same request selects the
more capable of those entries. With no hints and only `costPriority: 1.0`, the
example selects `fast` by affordability. If a `known_as` target is ineligible,
it is excluded before matching; ordinary hint matching and fallback still apply.
A server requiring an exact model cannot treat a sampling hint as a guarantee.
The sampling result reports the model that actually generated the response, not
the `known_as` name used to select it.

Implementation coverage MUST include alias precedence over both configured keys
and model ID matches; shared aliases resolved by priorities; first-match hint
ordering; eligibility filtering; absent and unmatched hints; omitted and zero
priorities; unrated models; deterministic ties; invalid configuration and request
values; and an empty eligible candidate set.

## Planning workflow mapping

The planning server supplies its own prompts, structured plan schema, tools, and
rendered plan resource. The host does not implement plan semantics.

| Current behavior | Protocol implementation |
| --- | --- |
| Planning interview | Initial context contains interview instructions and server-owned plan |
| `plan_update` | Ordinary MCP tool; server validates and atomically changes its plan |
| Protect active and terminal steps | Server-side plan validation |
| `plan_exit` | Tool result carries a replacement execution context with `next: "run"` |
| Restrict editing during planning | Planning context omits editing tools; execution context explicitly selects them |
| `plan_finish_step` completion | Server requests referee sampling before accepting completion |
| Referee approves | Persist summary/proof, advance, propose fresh execution context |
| Referee declines | Keep active step, propose fresh context containing the rejection reason |
| Skip step | Record reason and advance under the configured skip policy |
| Summarize completed work | Sampling over collected step evidence, stored in the plan |
| `plan_begin` | Replace context with planning instructions, blocker, and carried context; run to ask the user |
| Model stops with an active step | `stop` hook returns continuation content |
| Plan finishes | Permit final response and then wait |
| Clear model context | Plan persists on the server; the user's host session stays open |

The controller must not issue contradictory progression calls in one batch. The
server rejects incompatible plan mutations while a transition is pending. Shell
access during planning requires its own policy: blocking editor tools alone is
not a guarantee that files cannot change.

## Draft decisions still to resolve

These questions must be settled before calling version 1 an interoperable spec:

- Complete per-event and response JSON Schemas, extension error codes, and exact
  hook capability subfields for coverage and approval delegation.
- Portable tool inventory discovery and operation URI authority assignment.
- Durable stdio session resumption and how competing writers to the same
  controller MCP session are identified and explicitly taken over.
- Canonical target resolution for new files and operations whose targets depend
  on runtime results. Requested symlink paths and resolved destinations are
  already distinguished by `uri` and `canonicalUri`.
- Whether argument rewriting earns its complexity in version 1; this draft keeps
  execution arguments unchanged.
- Portable workspace binding for multi-root and remote execution environments.
  Durable server-owned session IDs and state are already requirements, not an
  optional retention policy.

## Review scenarios

An implementation should demonstrate these cases before claiming conformance:

1. Authorization and execution accept the same call parameter shape; describing
   a write does not perform it.
2. Read and search platform tools satisfy the same generic read policy; a mixed
   read/write or unknown-effect invocation does not inherit that permission.
3. A denied native edit never executes, including through a batch wrapper.
4. A required hook outage pauses execution; a user interrupt stops continuation.
5. `plan_exit` clears and seeds context without another user prompt.
6. Approval, rejection, skipping, and return to planning preserve server state.
7. A lost transition response is recovered without advancing a step twice.
8. A lost acknowledgment does not clear the host context a second time.
9. HTTP reconnection and server restart retain the same MCP session ID and plan;
   an unexpected missing session does not silently start an empty workflow.
10. The same server provides ordinary tools, resources, hooks, and sampling
    requests over one MCP transport without deadlock.
11. Static authorization causes no description RPC; `"authorize"` causes one per
    invocation. Missing metadata is not treated as permission.
12. `workspace-file:///**/AGENTS.md` matches root and nested instruction files,
    including in dot directories. A matching requested symlink path can resolve
    outside the root without being rejected solely for that reason.
13. Filesystem selectors reject nonempty authorities, including `localhost`.
14. Canonical destinations inside the canonical workspace use `workspace-file`,
    even for absolute requests; destinations outside it use hostless `file` URIs.
15. Planning model requests contain no editing tools. Enabling them requires a
    replacement execution context; returning to planning removes them through
    another replacement, never by modifying the active context's tool list.
16. Tool-list notifications and per-call denials do not mutate the active tool
    definitions. Guessed calls to tools outside the fixed selection never execute.
17. A replacement with `tools: "default"` restores the configured default
    selection; omission preserves the previous selection, and `[]` selects none.
    Later changes to host defaults leave the new context's selection unchanged.
