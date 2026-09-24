# frances-dev-mcp

A Deno MCP server for testing Frances. It provides `echo`, deliberate `fail`, cancellable `wait`,
and a persisted `increment` counter, plus the `dev-mcp:///fixture` JSON resource. It has no planning
workflow and advertises no Frances extensions.

Both transports use MCP 2026-07-28 with per-request metadata and `server/discover`. The small wire
implementation is deliberately explicit so protocol test cases can be added without an SDK hiding
responses. It supports JSON HTTP responses; it does not advertise subscriptions or streaming events.

From the repository root, use the supplied shared config with either transport:

```sh
frances --mcp packages/frances-dev-mcp/mcp.jsonc --mcp-server dev-http
frances --mcp packages/frances-dev-mcp/mcp.jsonc --mcp-server dev-stdio
```

Start the HTTP watcher or build the stdio script first, as described below.

## HTTP iteration

From the repository root:

```sh
just dev-mcp
just dev-mcp --port 3002 --state /tmp/frances-dev-state.json
```

The default endpoint is `http://127.0.0.1:3001/mcp`. Deno watches imported source files and restarts
the process on edits. The default state file is `target/frances-dev-mcp-state.json`; `--state`
overrides it. The state file is not a module or a watched input, so writes do not restart the
server. Restarting interrupts in-flight requests. Refresh the MCP selection in Frances after
changing tool definitions.

```toml
[mcp.servers.dev-http]
lifecycle = "modern"
transport = { type = "http", url = "http://127.0.0.1:3001/mcp" }
```

This is a local development endpoint, bound to loopback with Host and Origin validation. It has no
HTTP authentication and is not intended for public deployment.

## Executable stdio script

```sh
just build-dev-mcp
./target/frances-dev-mcp.js --stdio --state /tmp/frances-stdio-state.json
```

The build bundles all source into `target/frances-dev-mcp.js`, makes it executable, and embeds the
resolved absolute paths of both `env` and Deno in its shebang. `env -S` splits the runtime
arguments; neither executable is looked up through `PATH`. The worker can launch the script directly
without an active Nix shell, a checkout-relative working directory, npm dependencies, or a runtime
download. The artifact is machine-local: its pinned Nix store paths must exist on the executing
machine.

```toml
[mcp.servers.dev-stdio]
lifecycle = "modern"
transport = { type = "stdio", command = "/absolute/path/to/frances/target/frances-dev-mcp.js", args = ["--state", "/tmp/frances-stdio-state.json"] }
```

Use `local-stdio` instead to run on the host. Select the configured server through the MCP panel or
`frances --mcp-server dev-stdio`. Logs go to stderr; stdout is reserved for MCP messages.

## State

`--state PATH` loads a JSON file, creating it if absent:

```json
{
  "counter": 0,
  "fixture": { "example": "data returned by echo" }
}
```

Each increment serializes behind earlier mutations, writes a temporary file in the same directory,
flushes it, and atomically renames it over the state file before reporting success. A restart reads
the last committed snapshot. Invalid existing state fails startup instead of silently resetting it.
A save failure leaves the in-memory counter unchanged. Killing a process during a write can leave an
unused `.dev-mcp-*` temporary file; it is never treated as committed state.

Use one server process per state file; serialization covers concurrent requests within that process,
not competing processes. Use separate paths for simultaneous HTTP and stdio servers. Change fixture
data while stopped, then restart. Without `--state`, direct launches use ephemeral state and log
that choice. The HTTP watch task always supplies a state path.

This is atomic snapshot persistence, not exactly-once tool execution. A connection lost after a
successful save can leave the client uncertain about the result. Retrying `increment` increments
again. The file is synced before rename, but this does not promise directory-entry durability
through an OS crash or power loss.

## Checks

```sh
just check-dev-mcp
```

Tests cover HTTP requests, stdio cancellation, concurrent saves, startup recovery, invalid state,
and failed persistence. Run `just build-dev-mcp` again after editing sources for stdio testing; HTTP
uses the source directly.
