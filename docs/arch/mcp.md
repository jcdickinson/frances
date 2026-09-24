# MCP support

Frances supports ordinary MCP servers through `frances-mcp` and the Rust session
runtime. The Frances session, hook, UI, and database extensions remain draft
protocols and are not advertised by this implementation.

## Configuration and selection

Server definitions and presets live under `mcp` in the ordinary Frances
configuration, usually `~/.config/frances/config.toml`. Defining a server or
preset does not enable it. Native tools remain available independently.

```toml
[mcp.servers.docs]
transport = { type = "stdio", command = "my-docs-server", args = ["--stdio"] }

[mcp.servers.issues.transport]
type = "http"
url = "https://mcp.example.com/mcp"

[mcp.servers.issues.transport.headers]
Authorization = "Bearer ${ISSUES_TOKEN}"

[mcp.presets.rust]
servers = ["docs"]

[mcp.presets.project]
servers = ["docs", "issues"]
```

### Explicit JSON/JSONC source

`frances --mcp /path/to/mcp.json` loads a separate configuration source. The file can use
JSONC comments and trailing commas regardless of its extension. No `mcp.json` files are discovered
automatically. This source adapts the common `mcpServers` shape into the ordinary `mcp.servers`
configuration layer, after the other sources. It follows the existing per-field layering rules;
other configured servers and presets remain available.

```jsonc
{
  "mcpServers": {
    "worker-tool": {
      "command": "./target/frances-dev-mcp.js",
      "args": ["--state", "./target/dev-stdio-state.json"],
      "frances/place": "remote", // Run through the workspace worker.
    },
    "host-tool": {
      "type": "stdio",
      "command": "my-host-tool",
      "frances/place": "local",
      "env": { "TOKEN": "${TOKEN}" }
    },
    "dev-http": {
      "type": "http",
      "url": "http://127.0.0.1:3001/mcp"
    }
  }
}
```

`type` is optional when exactly one of `command` or `url` identifies the transport. For stdio,
`frances/place` defaults to `local` (native `local-stdio`); `remote` maps to native `stdio`.
The unprefixed `place` field belongs to other tools and has no effect in Frances. HTTP always
connects from the host: `frances/place` on an HTTP entry logs an error and is ignored, regardless
of its value.

Supported stdio fields are `command`, `args`, `env`, and `cwd`; HTTP supports `url` and `headers`.
Environment/header templates retain the native expansion behavior. Command, argument, and cwd
paths retain their execution-environment meaning; they are not rebased against the JSON file.
The `--mcp` file path itself is resolved relative to the launch directory and forwarded as an
absolute path when the app detaches.

This filename/schema convention is host-specific, not part of MCP itself. Shared files may contain
other tools' fields: Frances warns and ignores unknown fields (and accepts `$schema` without a
warning). Malformed or unsupported server entries log errors and are skipped independently. An
unreadable or malformed file logs an error and contributes no configuration; it does not stop the
app from starting. Diagnostics appear in the session log. Explicitly selecting a skipped server
still follows the ordinary unknown-server selection rules.

Loading definitions does not activate servers. Select them with the MCP panel, `--mcp-server`, or a
configured preset:

```sh
frances --mcp mcp.jsonc --mcp-server dev-http
```

`type = "stdio"` starts the process through the workspace worker. It inherits the
worker environment; environment templates are expanded there, and `which` runs
there using the resulting `PATH`. The host does not send its captured environment
to the worker. The default working directory is the primary workspace directory;
`transport.cwd` resolves against it on the worker.

Use `type = "local-stdio"` for a process on the Frances host, such as a desktop
integration. It uses the captured host environment and host filesystem. It accepts
the same `command`, `args`, `env`, and `cwd` fields. The working directory defaults
to the workspace directory on the host; configure `cwd` explicitly when needed.

Both transports pass executables and arguments directly, without a shell. Configured
environment entries override the inherited environment and expand with `EnvString`.
Relative executable paths and `PATH` entries resolve against the process working
directory. The shared async `which` helper in `frances-core` runs filesystem probes
on the executing machine's Tokio blocking pool. Missing or non-executable commands
fail before startup with an executable lookup error.

Worker stdin/stdout travel as bounded byte feeds. MCP negotiation remains in
Frances; the worker owns the child and stops it when the feeds close or the worker
connection shuts down. Both stdio transports send `SIGTERM`, wait up to
`shutdown_timeout_seconds` (default 5), then send `SIGKILL` if needed and reap
the child. Set that server-level option to 0 for immediate escalation. On Windows,
which has no Unix signals, shutdown terminates the child directly. Child stderr
goes to the executing machine's stderr, separate from MCP data.
HTTP connects from the host, and header templates use the host environment.

Compose presets at launch:

```sh
frances --preset rust+project
frances --preset rust --preset project --mcp-server another-server
```

The selected presets contribute the union of their servers, together with any
explicit server selections. Shared servers connect once. Unknown presets or
servers are errors. No workspace triggers run; those are a later feature.

The sidebar's MCP controls select multiple presets and additional servers.
Applying a selection interrupts the current turn, settles its tool results, and
prepares connections at a safe boundary. A failed preparation leaves the old
selection intact. Successful preparation starts a new model conversation with
the previous text history and fresh native tool state, including the editor read
cache. It preserves transcript entities and reuses retained server connections.
Applying the same selection refreshes its inventory into a new context.

## Connections and capabilities

Both stdio and Streamable HTTP use rmcp. The default `lifecycle = "auto"` probes
modern `server/discover` and falls back to classic initialization. A server can
set `lifecycle = "modern"` or `lifecycle = "legacy"` explicitly. Modern requests
carry the MCP 2026-07-28 per-request metadata. Legacy connections are ordinary
MCP only; connecting a server never grants workflow authority.

Servers can configure `startup_timeout_seconds` (default 30) and
`request_timeout_seconds` (default 120). Both must be positive. Requests that
execute tools, read resources, or fetch prompts propagate cancellation through
rmcp's request handles. A timeout or interruption does not imply that remote
effects were rolled back, and Frances does not automatically retry tool effects.
Core MRTR request-state continuations have a shared deadline and a 16-round cap.

HTTP authentication can be supplied through configured headers. Redirects are
disabled so those headers cannot silently move to another endpoint. Interactive
OAuth, sampling, elicitation, and roots are not advertised client capabilities.
Servers requesting unadvertised client input receive an explicit local failure.

## Tools, resources, and prompts

Tool discovery follows pagination and rejects duplicate names or invalid schemas.
Model tool names combine a readable prefix with a deterministic ID derived from
server identity, operation kind, and remote name. They satisfy provider name
limits and do not collide with native tools or another server's tools.

The context freezes its definitions. Legacy tool-list notifications and modern
tool-change subscriptions invalidate affected calls; they do not mutate model
definitions. Applying the selection again refreshes discovery. Lost connections
surface as failures, and explicit selection refresh can reconnect them.

MCP tool calls and resource reads require user approval through the existing
permission UI. Tool annotations are not treated as authorization facts, and MCP
does not use or modify `auto_judge`. Invalid arguments fail before approval or
execution. The server's `isError` flag reaches model history as a tool failure.

Servers with resources expose host-generated list/read tools. Listing includes
resource templates; reading passes the URI back to its owning server. Frances
does not interpret a remote resource URI as a local filesystem path. The command
palette's **Use MCP Prompt** action lets the user choose a server prompt and its
arguments; the resulting messages enter the conversation as user-selected content.

Full MCP results are retained as transcript entities. The model receives readable content rather than serialized MCP envelopes: text
blocks are passed through, resource reads show the URI and original text, catalogs
show resource names and URI templates, and prompts retain message roles. Protocol
metadata is omitted. Actual structured tool data remains JSON, without duplicating
an identical text block. The current provider
interface accepts text tool results: binary image/audio/resource data is retained
in the entity and replaced by an explicit unsupported-content notice in model
input. No media is silently presented as if the model had seen it.

## Extension boundary

The connection layer owns wire requests, capabilities, metadata, and lifecycle.
The session adapter owns tool selection, permission decisions, model projection,
and context replacement. Full tool declarations remain host-side with each call.
Future Frances extensions belong across that boundary; they do not require
putting session IDs, controller state, or authorization metadata in model arguments.

## Development server

[`packages/frances-dev-mcp`](../../packages/frances-dev-mcp/README.md) supplies a local test server.
`just dev-mcp` runs HTTP with source watching and persisted test state. `just build-dev-mcp` produces
an executable stdio script with absolute Nix runtime paths for worker-side testing.
