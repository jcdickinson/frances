# Model Content + Hooks Protocol

Status: draft protocol, not implemented. Extension version: 1. The prerequisite
native Rust harness is implemented without MCP or the planning workflow.

This document specifies a family of MCP extensions for hosts that run coding
agents. The working name is Model Content + Hooks Protocol. All extension
capabilities and RPC methods use the `frances/` namespace, but their contracts
must not depend on Frances, Rust, a particular model provider, or a particular
plan format.

The reference MCP baseline is
[2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28). Existing MCP
methods retain their meanings. Everything under `frances/` below is proposed here, not
part of the MCP standard. MUST, SHOULD, and MAY describe requirements of this draft.
Wire examples are illustrative instances of those requirements, not a complete
machine-readable schema.

These extensions require MCP 2026-07-28. Hosts may support earlier MCP revisions
for ordinary tools, resources, and prompts, but MUST NOT advertise or activate
these Frances extensions on those revisions. A selected workflow requiring them
fails explicitly if its server only supports classic MCP. No compatibility
handshake, session-header fallback, or legacy Frances wire format is specified.

## Naming conventions

Extension fields use camelCase, following MCP's naming conventions, for example
`canonicalUri` and `contextId`. Hook types are URIs, such as
`platform:///tool/use/before`, `platform:///session/start`, and
`frances:///plan/update`. RPC methods use slash-separated namespaces, such as
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
| Hook provider | Advertises and publishes its own events, subscribes to events, and handles deliveries |
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

Capabilities use `capabilities.extensions`, following MCP's
[versioning rules](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning).
The host obtains server capabilities through `server/discover` and advertises its
own capabilities in each request's `_meta["io.modelcontextprotocol/clientCapabilities"]`.
There is no `initialize` handshake or connection-scoped capability negotiation.

Each side advertises supported positive integer versions independently. The
capability fragment below can appear in discovery or client request metadata:

```json
{
  "capabilities": {
    "extensions": {
      "frances/authorization": { "versions": [1] },
      "frances/hooks": { "versions": [1] },
      "frances/session": { "versions": [1], "contextControl": true }
    }
  }
}
```

For each extension, use the highest version present in both advertisements.
No intersection means that extension is unavailable. Each request MUST advertise
the capabilities it relies on; neither endpoint may rely on earlier requests to
establish them. A server MUST NOT infer support from the client's product name.
A host MUST NOT silently substitute ordinary chat for a configured controller
whose required extensions are absent.

| Extension | Contract |
| --- | --- |
| `frances/authorization` | Describe a tool invocation without executing it |
| `frances/hooks` | Advertise URI hook types, manage subscriptions, and deliver platform and provider events |
| `frances/session` | Durable application identity, host-operation continuations, and optional context control |
| `frances/ui` | Publish semantic UI and receive user decisions; defined in the companion spec |
| `frances/db` | Optional host-provided storage; defined in the database companion |

`frances/session` combines session identity and context control in one extension.
`contextControl` defaults to false; context control is usable only when both sides
advertise true. Supporting sessions alone does not require hooks or context
replacement. Context control additionally requires `frances/hooks`, and only the
host-selected controller may propose transitions. `frances/hooks`, `frances/ui`,
and `frances/db` require `frances/session` for their durable identities.
Authorization descriptions can be implemented independently. A hook can operate
without descriptions, but MUST treat effects it cannot determine as unknown.
Ordinary MCP servers do not need any Frances extension.

## Sessions and persistence

These are application identities, independent of MCP transports:

| Identity | Meaning |
| --- | --- |
| Application session ID | Host-generated UUID identifying durable work on one configured server |
| Context ID | Host-issued identifier for one model conversation within that work |
| Revision | Host execution-state revision used to reject stale context transitions |

A session contains the server's plan, phase, step history, and other durable
state. A context contains the model conversation, fixed tool selection, and
transient execution state. Context replacement preserves the session and its
plan, creates a new context ID, and recreates context-local state such as the
editor read cache. These are separate identities within one extension.

The host maps `(host user session, configured server identity, authenticated
principal)` to an application session ID. The host's internal user-session ID
need not cross the wire. Different configured servers receive distinct IDs;
composing presets does not share their state. A stateless server that does not
support this extension receives no Frances session metadata.

### Creation and deletion

After discovery and explicit host selection, the host generates a UUID and
persists the mapping before sending `frances/session/create` with `{ "id": ... }`.
This is a host-to-server RPC, not a model-facing tool. Its successful result is
`{ "resultType": "complete", "id": ... }`. Creation MUST atomically persist the
session and its creation receipt before returning. An identical retry by the same
authenticated owner returns the original result; conflicting creation parameters
fail. The ID is the creation idempotency key, not the JSON-RPC request ID.

`frances/session/read` has no method-specific parameters and returns
`{ "resultType": "complete", "id": ... }` for the session identified by metadata.
It checks that the saved session is available without creating or attaching one.
`frances/session/delete` likewise selects the session through metadata and returns
`{ "resultType": "complete", "deleted": true }`. Deletion requires explicit host
authorization. Repeating it succeeds for the same owner. Servers retain a deletion
tombstone so delayed creation retries cannot resurrect a deleted session. Deletion
and ordinary operations serialize: once deletion succeeds, no pending operation
may recreate or modify that session. New work always uses a new UUID.

Missing, unknown, deleted, or unauthorized IDs MUST fail on ordinary requests;
they MUST NOT create a session implicitly or fall back to an unscoped operation.
Errors MUST NOT disclose another owner's state. Application errors use a
machine-readable `data.kind`; session kinds include `sessionNotFound`,
`sessionDeleted`, `sessionConflict`, and `permissionDenied`. Numeric extension
codes must be outside the JSON-RPC reserved range, following the MCP baseline.

### Metadata on every request

Once the host opts into sessions for a server, its request layer MUST attach
`_meta["frances/session"]` to **every RPC issued for that application session**,
including ordinary MCP tool, resource, prompt, discovery, and subscription RPCs,
as well as Frances methods. There is no per-tool opt-in. Initial discovery and
creation precede the session and omit this metadata; creation carries the new ID
in its method parameters. Server-wide discovery outside a user session can also
remain unscoped. Method-independent metadata does not change standard MCP tool
arguments or permit session-dependent tool catalogs contrary to the base spec.

The metadata contains `id`. When context control is negotiated, it additionally
contains the current `contextId` and `revision` together. Every session-bound tool
call also carries a stable `invocationId` so it can support execution deduplication
and host-operation continuations. These fields
are generated by the host, never accepted from model-generated arguments. A
session-only provider needs no context fields and gains no context-control authority.

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "tools/call",
  "params": {
    "name": "plan_next_step",
    "arguments": {},
    "_meta": {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientInfo": { "name": "example-host", "version": "1" },
      "io.modelcontextprotocol/clientCapabilities": {
        "extensions": {
          "frances/session": { "versions": [1], "contextControl": true },
          "frances/hooks": { "versions": [1] }
        }
      },
      "frances/session": {
        "id": "7dc95b0b-2664-49ab-a3ab-246435485bf8",
        "contextId": "context-7",
        "revision": 12,
        "invocationId": "call-9"
      }
    }
  }
}
```

Other examples abbreviate standard request metadata and session identity. Empty
parameters mean no method-specific parameters, not absence of required `_meta`.
All completed RPC results include `resultType: "complete"`; fragments may omit
that envelope. HTTP requests also carry the standard routing/version headers.
No `Mcp-Session-Id` header is used. This application metadata works identically
over HTTP and stdio.

Connections and stdio processes may serve multiple application sessions. The
sender chooses the binding per request, and the receiver MUST NOT infer it from
the connection, process, or most recently seen ID. Responses inherit their
request's binding; subscription notifications are correlated by MCP's
`io.modelcontextprotocol/subscriptionId`. A long-lived subscription retains its
original session binding until cancelled; it does not change when another
session sends a request over the same connection.

### Durability and recovery

Servers MUST retain the session and application state across disconnects and
process restarts until explicit deletion, without idle expiry. Closing the app,
deselecting a preset, or replacing context does not delete the session. The
server chooses its database; the host need not store plan snapshots.

On reconnect, the host rediscovers capabilities and uses its saved ID. It reads
the session, reconciles pending controller transitions, restores subscriptions,
and delivers `platform:///session/start` with reason `resume` before model execution.
An unknown saved session is an error, never permission to create an empty plan.
HTTP and stdio use the same recovery procedure.

The server MUST authorize the authenticated principal on every request; knowing
an ID is not authorization. Local stdio deployments use their host-established
security boundary. Concurrent writers to a controller session are unsupported;
hosts MUST NOT run two writers against the same ID. Controller takeover requires
an explicit ownership mechanism, still a draft question.

The host persists context identity, revision, and transition receipts alongside
the session mapping. Ordinary tool calls are not promised exactly-once execution
after a network failure. Creation, deletion, controller invocation, and transition
acknowledgment have the explicit retry contracts specified here.

## Host operations and continuations

MCP 2026-07-28 forbids independent server-initiated JSON-RPC requests. Its
[MRTR pattern](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr)
permits `input_required` only on `tools/call`, `prompts/get`, and `resources/read`,
and defines a closed set of input-request methods. Arbitrary database/UI methods
and hooks MUST NOT be presented as standard MRTR.

Instead, negotiated `frances/session` version 1 defines the extension result type
`frances/hostInputRequired`, using MCP's allowance for extension-defined result
types. It is permitted on `tools/call`, `frances/hooks/invoke`,
`frances/hooks/changed`, and `frances/ui/respond`. It contains an opaque
`requestState` and a nonempty
`hostRequests` map. Each value is `{ method, params }`, without a JSON-RPC ID or
envelope. Methods are limited to negotiated `frances/db/*` and the UI
`frances/ui/publish` and `frances/ui/remove` operations, plus negotiated
`frances/hooks/subscribe`, `frances/hooks/unsubscribe`, and
`frances/hooks/publish`. Negotiated sampling can
also be requested as `sampling/createMessage` with its standard parameters.
This envelope extends result handling, not the core MRTR input-request union.

The host executes these operations under its policy, then repeats the originating
RPC with a new JSON-RPC ID. Its params retain the original semantic arguments and
add `_meta["frances/session"].continuation` containing the exact `requestState`
and a `hostResponses` map. Each entry is either `{ "result": ... }` or
`{ "error": { "code": ..., "message": ..., "data": ... } }`. Keys match
`hostRequests`; the host does not invent successful responses for unsupported or
denied operations. Host-request keys MUST remain stable for retries and MUST NOT be reused for
different operations within an originating call. Entries are independent;
dependent operations require another round. This is not a distributed transaction.

```json
{
  "resultType": "frances/hostInputRequired",
  "requestState": "opaque-server-state",
  "hostRequests": {
    "publish-review": {
      "method": "frances/ui/publish",
      "params": {
        "id": "approve-plan",
        "revision": 1,
        "presentation": "modal",
        "body": { "type": "review", "state": "pending", "title": "Approve plan",
          "artifact": { "id": "plan", "revision": 7 }, "blocking": true }
      }
    }
  }
}
```

For that result, the host retries the originating method and arguments with this
additional session-metadata field (alongside the unchanged session and operation
identifiers):

```json
{
  "continuation": {
    "requestState": "opaque-server-state",
    "hostResponses": {
      "publish-review": { "result": { "status": "accepted" } }
    }
  }
}
```

Host operations inherit the authenticated server and application session from the
originating RPC. An embedded operation cannot select another session or principal.
The host MUST service them while the outer operation is pending, without holding
locks that prevent storage, UI, interruption, or sampling. Publication returns
acceptance of presentation promptly; it never waits for a human answer. Human
actions use a later `frances/ui/respond` RPC.

The server binds continuation state to the authenticated owner, application session,
originating method, semantic arguments, and stable operation ID. It MUST verify
integrity and reject cross-request reuse. Continuations preserve the original
`invocationId`, `eventId`, `updateId`, or `actionId` (required for any call using this continuation,
including a session-only tool provider); continuation payloads are excluded from
semantic-argument comparisons for deduplication. The server MUST track intermediate
versus terminal outcomes and MUST NOT repeat committed effects when a round is retried.
The host keys completed host-operation responses by application session, stable
originating operation ID, and host-request key, and persists them before continuing;
database mutations additionally use their durable `requestId`. Neither side may assume
that the next round will arrive.

Hosts bound rounds, time, and resource use and surface exhaustion as an error.
Interruptions stop automatic continuation; uncertain committed effects are
reconciled before resumption. Context identity and revision remain those of the
originating operation during its continuation; the host compares proposed
transitions with its current revision before applying them. A continuation token
is temporary request state, not the durable session ID or a replacement for
pending-transition recovery. All continuation metadata stays out of model input.

## Authorization descriptions

### Tool declarations: static or per-call

MCP tool definitions provide `_meta` for extension metadata and `inputSchema` for their
argument schema. This extension uses `_meta["frances/authorization"]`, not a new
top-level `extensions` property. See the [MCP Tool
schema](https://modelcontextprotocol.io/specification/2026-07-28/schema#tool) and
[metadata key
rules](https://modelcontextprotocol.io/specification/2026-07-28/basic#general-fields).

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
another tool. Advertised capability support alone MUST NOT trigger lookups for
every tool. Missing metadata means unknown effects. Invalid metadata or a failed
advertised lookup is an error, not a fallback to a more permissive description.
Without negotiated extension support, this metadata grants no permission.

The host invalidates cached declarations when the tool list changes or the server
server capabilities or catalog freshness change. This refreshes the inventory for future contexts;
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

The requests have independent JSON-RPC IDs. Required MCP and session metadata
are supplied under the same parameter schema. A description MUST NOT execute the
tool, mutate the requested resources, or advance workflow state. It may inspect
metadata needed to resolve targets. Unknown tools and invalid parameters produce
ordinary JSON-RPC errors.

The separate MCP Tasks extension can add task metadata. Negotiating task support for
`tools/call` does not authorize task augmentation of this extension method. A version 1
receiver MUST reject task-augmented description requests explicitly; the eventual
task-enabled execution is still subject to completed authorization. This is a method
capability restriction, not a different parameter schema. See [MCP
CallToolRequestParams](https://modelcontextprotocol.io/specification/2026-07-28/schema#calltoolrequestparams).

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

## Hooks

### Identities and scope

Hook types are absolute URIs: `platform:///session/start` is a platform event,
and `frances:///plan/update` is an event published by a configured MCP provider.
The scheme identifies the publisher namespace; the slash-separated path identifies
the event type. Hook URIs use an empty authority, an absolute nonempty path, and
no query or fragment. Schemes match `[a-z][a-z0-9+.-]*`. Path segments contain
ASCII letters, digits, `_`, or `-`; empty, `.` and `..` segments and percent
encoding are rejected. This makes exact URI equality sufficient for matching.
These are identifiers, not URLs to fetch or filesystem operation URIs.

The host reserves `platform` and assigns each configured publisher a unique URI
scheme, such as `frances`. A server advertises hooks in its assigned namespace;
it cannot claim another provider's namespace or publish platform events. Multiple
configurations of the same server need distinct assigned namespaces. The host
passes `namespace` in `frances/hooks/list` so a provider need not assume its alias.

The subscription key is **(subscriber MCP identity, hook URI)** within one host
user session. There is no handler ID or subscription ID. A subscriber that wants
multiple local handlers dispatches to them itself. Repeated subscriptions are
idempotent and never cause duplicate deliveries.

The host routes events only within that host user session. Each recipient receives
its own application session metadata; the publisher cannot choose a recipient's
session ID. Host policy controls visibility and subscription permissions. A server
cannot make itself a required subscriber or gain controller authority by subscribing.

### Up-front advertisement and initial subscriptions

After application session creation or recovery, the host calls
`frances/hooks/list` with `{ "namespace": "frances" }`. The provider returns its
complete published hook catalog and the hooks it wants to subscribe to:

```json
{
  "resultType": "complete",
  "hooks": [
    {
      "uri": "frances:///plan/update",
      "description": "The saved plan changed.",
      "payloadSchema": { "type": "object" }
    }
  ],
  "subscriptions": [
    "platform:///session/start",
    "engineering:///review/complete"
  ]
}
```

`hooks` describes what this provider can publish, not its local handlers.
Descriptors contain `uri`, `description`, and an object-valued JSON Schema
`payloadSchema` for the event payload. Published types are advertised up-front
and remain fixed for that provider's application session. Reloading or reconnecting
a publisher reconciles its catalog before new publications are accepted.
Subscriptions may change throughout the session.

The host installs valid advertisements and processes each initial subscription
independently on application session creation, using the same rules as
`frances/hooks/subscribe`. On recovery, persisted subscription intent takes
precedence over this bootstrap list, so reconnecting does not undo a later
unsubscribe. The provider can request changes after its recovery snapshot. A
provider may request every hook it could ever need before receiving any platform catalog.
The host MUST NOT require publisher-first load order or a successful catalog
notification before accepting subscription intent. Individual unavailable hooks
do not fail startup or prevent other subscriptions from succeeding.

### Subscription intent and availability

`frances/hooks/subscribe` is a host operation with `{ "uris": [...] }`:

```json
{
  "method": "frances/hooks/subscribe",
  "params": {
    "uris": [
      "platform:///session/start",
      "engineering:///review/complete"
    ]
  }
}
```

Its result contains `subscriptions`, one outcome per requested entry in input
order. Duplicate URIs return the same outcome without adding subscriptions; an
empty list is a successful no-op:

```json
{
  "subscriptions": [
    { "uri": "platform:///session/start", "state": "active" },
    {
      "uri": "engineering:///review/complete",
      "state": "pending",
      "error": {
        "kind": "hookNotAvailable",
        "message": "The publisher has not advertised this hook."
      }
    }
  ]
}
```

An unavailable hook returns a non-fatal error **and retains subscription intent**.
When an authorized publisher becomes available, the host establishes the pending
subscription automatically. When the publisher becomes unavailable, the subscription
returns to pending without losing intent. Retrying subscribe returns the current
state; it neither creates another handler nor clears the pending request.

Malformed or forbidden requests return a per-URI `state: "rejected"` with an
`error` carrying `kind` and `message`; they do not create subscription intent.
Permission failures must not disclose hidden publishers. Envelope validation
errors remain ordinary operation errors. Errors for one URI do not roll back
successful requests for other URIs.

`frances/hooks/unsubscribe` takes `{ "uris": [...] }` and returns
`{ "unsubscribed": [...] }`. It removes both active subscriptions and pending
intent. Removing an absent subscription succeeds. The authenticated originating
provider is always the subscriber; it cannot unsubscribe another provider.
The list is validated before removal; invalid URI syntax fails without removing
any subscriptions. The result echoes the requested URIs in input order, including
duplicates; an empty list is a successful no-op.

Subscription intent and operation receipts are durable. Changes serialize at the
host, and a replayed continuation returns its recorded outcome without undoing
a later unsubscribe. Transient delivery failure does not silently unsubscribe
a provider. Host-required policy remains required even when its provider is
unavailable; pending status is not permission to bypass it.

### Full catalog updates

`frances/hooks/changed` is a host-to-provider request carrying `updateId`, an
increasing `revision`, `hooks`, and `pendingSubscriptions`. It is a complete
snapshot for that recipient, not a diff:

```json
{
  "updateId": "hook-update-8",
  "revision": 8,
  "hooks": [
    {
      "uri": "platform:///session/start",
      "description": "The application session starts or resumes.",
      "payloadSchema": { "type": "object" },
      "subscribed": true
    },
    {
      "uri": "frances:///plan/update",
      "description": "The saved plan changed.",
      "payloadSchema": { "type": "object" },
      "subscribed": false
    }
  ],
  "pendingSubscriptions": [
    {
      "uri": "engineering:///review/complete",
      "error": { "kind": "hookNotAvailable", "message": "Hook unavailable." }
    }
  ],
  "rejectedSubscriptions": []
}
```

`hooks` includes every available hook visible to this recipient, each with one
`subscribed` boolean indicating whether its subscription is established. There
is no array of handler IDs. `pendingSubscriptions` lists all retained intents
that are not established, with their current errors. Initial subscription
rejections are additionally reported as `rejectedSubscriptions` on the initial
update, using the rejected outcome shape above; otherwise that array is empty.

The host MUST deliver at least one snapshot after processing the provider's
up-front advertisement and subscriptions, before delivering its initial platform
session-start event. It may send snapshots at any time and MUST send a fresh
snapshot when the visible catalog or recipient's subscription state changes.
This includes publisher arrival, removal, and reconnect. Updates may coalesce
intermediate changes; every update is a complete current snapshot.

The provider returns `{ "resultType": "complete" }` after recording the snapshot.
It can also use `frances/hostInputRequired` to subscribe or unsubscribe in response.
That continuation retains the original `updateId`. Repeating an already established
subscription changes no state and MUST NOT itself generate another changed update.
Actual changes queue a subsequent snapshot after the current update completes.

Snapshots are serialized per recipient. Retries preserve ID, revision, and content;
stale snapshots cannot overwrite newer state. The host retains the latest snapshot
and pending receipt for recovery. It sends the relevant snapshot before delivering
an event through a newly established subscription. This does not replay events
published before the subscription became active.

### Platform events

| Event | Delivery point | Permitted response |
| --- | --- | --- |
| `platform:///session/start` | After application session creation or recovery and pending-transition reconciliation, before model execution | Context, controller transition |
| `platform:///context/start` | After replacement, before the next model request | Context only |
| `platform:///prompt/submit` | Before adding new user input | Accept or deny with reason, context |
| `platform:///tool/use/before` | After argument validation and description, before execution | Authorization decision, context |
| `platform:///tool/permission` | When host policy requires approval | Authorization decision |
| `platform:///tool/use/after` | After one call settles, including failures or denials | Context, controller transition |
| `platform:///tool/use/batch/after` | After all calls in a model response settle | Context, controller transition |
| `platform:///stop` | Model would finish without further tool calls | Wait, continue, or controller transition |
| `platform:///interrupt` | User interrupts execution | Observation only |
| `platform:///session/end` | Host detaches from the session | Observation only |

The names and tool decision points draw on
[Claude Code hooks](https://code.claude.com/docs/en/hooks#pretooluse-decision-control)
and [Codex hooks](https://learn.chatgpt.com/docs/hooks). This draft defines its own
coverage and semantics rather than inheriting either host's implementation gaps.
`platform:///session/end` does not delete server state.

### Provider broadcasts

`frances/hooks/publish` is a host operation with `publishId`, `uri`, and `payload`:

```json
{
  "method": "frances/hooks/publish",
  "params": {
    "publishId": "plan-update-19",
    "uri": "frances:///plan/update",
    "payload": { "planId": "plan-1", "revision": 19 }
  }
}
```

The host validates publisher ownership, advertisement, payload schema, and policy.
It persists acceptance and queues delivery to currently established subscribers,
then returns `{ "status": "accepted", "eventId": ... }`. Acceptance does not
mean subscribers have handled the event. No subscribers is a successful publication.
An unadvertised event is a `hookNotAvailable` error; publication does not implicitly
advertise a hook or create pending subscription intent.

The deduplication key is `(publisher application session, publishId)`. Identical
retries return the same event ID; conflicting reuse fails. The host records the
recipient set at acceptance. Newly established subscriptions receive future events,
not earlier publications. Unsubscribing removes undelivered work for that
subscription; it cannot undo an invocation already dispatched. Unsubscribing and
subscribing again must not resurrect cancelled deliveries.

Delivery uses `frances/hooks/invoke` with a host-issued `eventId`, host-established
`publisher` identity, and `event: { "type": <hook URI>, "payload": ... }`.
The publisher's application session ID is not copied into recipient metadata.
Custom broadcasts report occurrences; subscriber responses cannot veto or undo
the publisher's completed operation. They may provide context and, for the selected
controller only, propose a context transition under the existing revision rules.
They cannot return platform authorization decisions or claim platform provenance.

The host MUST queue broadcasts rather than synchronously wait for subscriber
responses inside `publish`. It services the originating continuation first and
delivers events at safe boundaries. Queued events retain their originating
evaluation stack as specified below. Hosts also bound event chains and queue
growth and surface exhaustion explicitly. Protocol bookkeeping calls do not
themselves emit hook events.

For publication while no client call is active, a provider MAY additionally return
`eventResourceUri` from `frances/hooks/list`. It identifies a session-scoped MCP
resource containing `{ "events": [...] }`, a durable outbox of the same
`{ publishId, uri, payload }` envelopes in publication order. The host subscribes
before its final resource read, as in the UI extension's background-update pattern,
then reads on notifications or polls when subscriptions are unavailable. The host
discloses polling delays. The resource must remain readable until acknowledged;
the URI does not itself authorize access to another session's events.

Each outbox event goes through the same publication validation and deduplication.
After persisting its outcome, the host sends `frances/hooks/ack` with
`{ "publications": [{ "publishId": ..., "outcome": ... }] }`. Each outcome is
either `{ "status": "accepted", "eventId": ... }` or
`{ "status": "rejected", "error": { "kind": ..., "message": ... } }`.
The provider records receipts before returning `{ "resultType": "complete" }`
and can then remove those outbox entries. Identical acknowledgments succeed;
conflicting acknowledgments fail. Lost acknowledgments cause replay, not a new
broadcast. A changed envelope with an existing publish ID is a protocol error,
not a new outcome for an already acknowledged publication.

Background publication does not by itself restart the model. Context is queued
for the next permitted model request; automatic continuation still requires an
authorized controller transition. Interruption, pending user decisions, and new
user input retain their existing precedence.

### Evaluation stack and recursive delivery

The host maintains an evaluation stack for each causal chain of hook invocations.
A frame is `(subscriber MCP identity, hook URI)`, scoped to the host user session.
The publisher, event ID, local handler, and skill activation are not part of the
key. A provider cannot evade the recursion check by publishing a new event ID.

Before invoking a subscriber, the host checks the entire inherited stack:

1. If that key is already present, skip this invocation and log a warning with
   the subscriber, hook URI, event ID, and evaluation stack.
2. Otherwise, push the key, invoke the subscriber, and pop the frame on completion,
   error, or cancellation. Continuation rounds belong to the same invocation and
   keep its frame; they are not recursive hook invocations.

Skipping is non-fatal and affects only that recipient. Other subscribers still
receive the event. The skipped invocation contributes no decision or context;
it is never recorded as an allowance or successful execution of a required hook.
If an operation needs an affirmative decision, a skipped callback cannot supply it.

This is a stack, not a session-wide visited set. A hook may run again after its
earlier invocation is no longer an ancestor. Sibling deliveries inherit separate
copies of their parent's stack; an unrelated event starts with an empty stack.
Different MCPs may handle the same hook, and one MCP may handle different hooks,
until a branch would repeat an existing pair.

Every event published during evaluation captures the current stack, including
the publishing subscriber's frame. The host retains that stack with queued work
and restores it for delivery, including after restart. Popping the live frame
when the originating RPC returns does not erase queued events' ancestry. This
prevents deferred cycles as well as direct recursion.

For example, A is the engineering provider: it handles `frances:///plan/update`
and publishes `engineering:///review/complete`. B is the Frances provider: it
handles that event and publishes another `frances:///plan/update`. Delivery to A
is skipped because
`(A, frances:///plan/update)` already appears in that branch's stack. Delivery
to another subscriber continues if its pair is absent.

The host attaches an opaque `evaluationId` under `_meta["frances/hooks"]` on
each invocation and maps it to the current stack and recipient session. It
retains that mapping for resumable delivery. Host-operation publications inherit
it automatically. Outbox events caused by that invocation MUST additionally carry
its `evaluationId`; independent background events omit it and start a new chain.
The host validates an echoed ID against the publishing provider and session and
loads its saved stack. Unknown or cross-session IDs fail explicitly. Providers
cannot supply stack frames themselves. This is cooperative causal tracking, not
an execution sandbox for a provider that hides an event's origin.

Retries use the saved inherited stack and invocation receipt, not a second push
onto a still-running invocation. The host deduplicates or serializes concurrent
retries. Stack checks run before dispatch and never recursively emit another
hook event merely to report the skip.

### `frances/hooks/invoke` — host to hook provider

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "frances/hooks/invoke",
  "params": {
    "_meta": { "frances/hooks": { "evaluationId": "evaluation-18" } },
    "eventId": "event-18",
    "publisher": { "kind": "platform" },
    "event": {
      "type": "platform:///tool/use/before",
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

An MCP provider is represented as `{ "kind": "mcp", "id": "ferrisfetch" }`. The host
supplies this identity. `publisher` identifies the event producer; the `provider`
inside a platform tool event identifies the tool executor. The outer request
carries the subscriber's session binding;
context identity and revision also come from that metadata. `call` preserves the target
provider's MCP call parameters, including metadata; native tools are adapted to that
shape. This hook envelope does not change the separate authorization request's parameter
shape.

Other event payloads carry the data needed at their delivery point:

- `platform:///session/start`: reason `new` or `resume`.
- `platform:///context/start`: applied transition ID and reason.
- `platform:///prompt/submit`: submitted MCP content blocks.
- `platform:///tool/permission`: invocation, authorization description, and host approval reason.
- `platform:///tool/use/after`: invocation ID, final call, and tagged outcome: result, denied,
  cancelled, or execution error. A result carries an MCP `CallToolResult`.
- `platform:///tool/use/batch/after`: settled invocation IDs in model order and assistant content.
- `platform:///stop`: final assistant content and the count of automatic continuations.
- `platform:///interrupt` and `platform:///session/end`: reason.

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

`platform:///tool/use/before` allowance permits the call through workflow policy; it does not
override host permissions or sandboxing. `platform:///tool/permission` allowance can answer
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
hooks. Hosts must still service negotiated host-operation continuations while
awaiting a hook; servers cannot initiate a separate sampling RPC.

`platform:///stop` responses choose `wait` or `continue`; `continue` includes model-facing
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

## Context control within `frances/session`

A controller with negotiated `contextControl: true`, selected by the host, can
propose a transition in a completed hook, tool, or `frances/ui/respond` result
under `_meta["frances/session"].transition`. Keeping the transition with the final
result ensures that `plan_exit` does not race a separate clear notification.
Intermediate host-operation results MUST NOT carry a transition.
UI responses require negotiated `frances/ui`, for example after explicit plan
approval. The same authority, revision, and acknowledgment rules apply.

```json
{
  "resultType": "complete",
  "content": [
    {
      "type": "text",
      "text": "Planning complete."
    }
  ],
  "_meta": {
    "frances/session": {
      "transition": {
        "transitionId": "transition-4",
        "expectedRevision": 12,
        "action": {
          "type": "replace",
          "reason": "Begin the first execution step",
          "instructions": "Execute only the active step. Submit proof when finished.",
          "content": [
            {
              "type": "text",
              "text": "# Agreed plan\n\n..."
            }
          ],
          "next": "run"
        }
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
during `platform:///session/start`, before the first model call.

The host resolves the selection against its inventory and permissions before
accepting the context, then freezes the model-facing tool names, descriptions,
and schemas for that context. Disabled tools MUST NOT appear in the model's tool
definitions, tool discovery results, or callable surfaces exposed by wrappers.
The same selection applies to every model request within the context.

Changing that selection requires an explicit context replacement with a new
context ID. There is no per-model-call tool filtering hook and no in-place tool
enable/disable operation. `platform:///context/start` can add context but cannot change the
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

The host attaches `contextId` and `revision` to calls to the controller alongside the
application session `id` in `_meta["frances/session"]`, without changing model-facing
arguments. A stable `invocationId` in that metadata identifies execution retries. The
authorization description receives the same call parameters.

The server persists a proposed transition before returning it. It MUST retain
unacknowledged proposals and expose them through `frances/session/context/pending`.
Controller tool calls
that mutate workflow state MUST deduplicate by invocation ID and return the
recorded result for a retry; the same ID with different parameters is an error.
This requirement does not imply idempotence of arbitrary external tools.

`frances/session/context/pending` is a host-to-controller request with empty parameters.
It returns `{ "resultType": "complete", "transitions": [...] }`, containing
unacknowledged proposals in creation order for the current application session. It is
read-only and does not create, attach, or replace a session. After reconnection, the
host retrieves and reconciles these proposals before delivering `platform:///session/start` or
resuming model execution.

The host MUST:

1. Record the triggering tool result and settle or cancel every outstanding call
   in that model batch. Do not switch context while a call is unaccounted for.
2. Validate controller authority and the expected revision. Conflicting proposals
   cannot be applied in sequence merely because they arrived in sequence.
3. Persist acceptance, the new context identity, and continuation intent before
   issuing another model request.
4. Apply the replacement and deliver `platform:///context/start`.
5. Acknowledge the transition with `frances/session/context/ack`.
6. Run if requested, unless interrupted or awaiting user input.

`frances/session/context/ack` has parameters `transitionId` and `outcome`. The outcome is
either `{ "type": "applied", "contextId": ..., "revision": ... }` or
`{ "type": "rejected", "reason": ... }`; its result is `{ "resultType": "complete" }`. Duplicate
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

Use the MCP [sampling](https://modelcontextprotocol.io/specification/2026-07-28/client/sampling)
request and result shapes where negotiated. Standard tool calls can request it
through core MRTR; hooks use the `frances/hostInputRequired` continuation above.
No standalone server-to-host sampling RPC is sent. Sampling is deprecated in the
2026-07-28 baseline but still supported there; this draft retains it for referee
and summarizer behavior. Its eventual replacement remains a design question.
The host owns model selection, sampling permissions, and execution budgets.

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
| Model stops with an active step | `platform:///stop` hook returns continuation content |
| Plan finishes | Permit final response and then wait |
| Clear model context | Plan persists on the server; the user's host session stays open |

The controller must not issue contradictory progression calls in one batch. The
server rejects incompatible plan mutations while a transition is pending. Shell
access during planning requires its own policy: blocking editor tools alone is
not a guarantee that files cannot change.

## Draft decisions still to resolve

These questions must be settled before calling version 1 an interoperable spec:

- Complete per-event and response JSON Schemas, extension error codes, and exact
  hook capability subfields for coverage and approval delegation, including
  catalog snapshots, pending subscriptions, broadcast receipts, and evaluation
  metadata.
- Portable tool inventory discovery and operation URI authority assignment.
- How competing writers to the same controller application session are fenced
  out and explicitly taken over. HTTP and stdio session resumption are specified
  through application metadata above.
- Complete continuation schemas and the long-term replacement for deprecated
  MCP sampling; continuation operation failures must remain explicit.
- Canonical target resolution for new files and operations whose targets depend
  on runtime results. Requested symlink paths and resolved destinations are
  already distinguished by `uri` and `canonicalUri`.
- Whether argument rewriting earns its complexity in version 1; this draft keeps
  execution arguments unchanged.
- Portable workspace binding for multi-root and remote execution environments.
  Host-issued application session IDs and durable server-owned state are already requirements, not an
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
9. HTTP or stdio reconnection and server restart retain the same application session ID and plan;
   an unexpected missing session does not silently start an empty workflow.
10. The same server provides ordinary tools, resources, hooks, and sampling
    continuations over one MCP transport without deadlock or independent server RPCs.
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
18. Two application sessions share a connection without mixing plans, hooks, UI,
    or database bindings; every session RPC carries the correct metadata.
19. Creation retries return the same session. Deletion retries succeed, and stale
    creation or mutation retries cannot resurrect deleted state.
20. A session-only provider works without context control; advertising context
    support does not grant controller authority.
21. A lost continuation response recovers without repeating committed host
    mutations. Interrupting between rounds never starts another model turn.
22. A provider subscribes before seeing a catalog or before its publisher loads.
    The request reports a non-fatal unavailable error, retains pending intent,
    and becomes established automatically when the publisher advertises the URI.
23. Every provider receives a full catalog snapshot at least once. Publisher
    changes and subscription changes update that recipient's established and
    pending state; repeated identical subscriptions create neither duplicate
    deliveries nor an endless catalog-update loop.
24. Two subscribers handle the same URI independently. Multiple handlers inside
    one subscriber require only one protocol subscription. Unsubscribe removes
    pending intent as well as established subscriptions.
25. Provider broadcasts reach established subscribers without borrowing platform
    authority or leaking another provider's application session ID. Publication
    retries and outbox acknowledgment retries do not broadcast an event twice.
26. A direct or indirect cycle skips only the repeated `(MCP, hook URI)` pair
    and logs a warning. Other recipients continue. Siblings and later independent
    events can invoke that pair again.
27. Queued delivery and restart retain causal stacks. Continuation rounds and
    execution retries neither bypass cycle detection nor falsely count as new
    recursive invocations. Outbox events caused by a hook retain its evaluation ID.
28. A batch subscription request reports active, pending, and rejected entries
    independently in input order. Duplicate URIs add no handlers, empty lists
    succeed, and batched unsubscribe removes both established and pending intent.
