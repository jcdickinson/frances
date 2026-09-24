# Frances

Frances is an agentic coding tool with a Tauri desktop app, and a worker.

- No TUI: The main advantage of a TUI is that it's trivial to get working remotely (just run it in SSH), but it causes
  several challenging UI problems; I always point at font-size as one example - too many things aren't important enough
  to justify font size equality.
- Worker: I work around the lack of a TUI by using a remote worker, vscode-style.
- Native Rust harness: ordinary chat with filesystem, editing, search, shell, and variable tools.
- Planned MCP extensions: [Model Content + Hooks Protocol](docs/model-content-hooks-protocol.md) describes future hooks and context management. MCP is not implemented yet.

## The name

Frances is named for two early UNIVAC programmers:

- **Frances E. Holberton** (1917–2001) — one of the six original ENIAC programmers, who went on to work on UNIVAC.
- **Frances ("Betty") Morello** — a UNIVAC programmer of the same era.

> [!WARNING]
> **This project is almost entirely coded by an LLM.**

```toml
[model_providers.deepseek]
kind = "deepseek"
base_url = "https://api.deepseek.com"
auth = { file = "/home/you/.config/frances/ds.txt" }

[models.default]
model_provider = "deepseek"
id = "deepseek-chat"

```

## Building

The workspace pins a single Rust toolchain in `rust-toolchain.toml`
(1.95.0, edition 2024).

```bash
cargo build                  # build everything
cargo build -p frances       # just the binary
cd packages/frontend && deno task build
deno task --config packages/frontend/deno.json app
cargo nextest                # run all tests
cargo fmt --all
cargo clippy --all-targets
nix build                    # reproducible build via flake.nix
```

A `nix develop` dev shell provides the toolchain plus `rust-analyzer`, `jq`,
`python3`, and `cargo machete` (the unused-dependency check).

## Running

```bash
frances                 # open the current directory; launch in the background
frances path/to/repo    # open a directory as a workspace
frances ws.toml         # open a workspace file: dirs = ["a", "b"]
frances install         # write starter model/provider configuration
frances --foreground    # run attached (useful for development)
frances --export-tool-schemas > tool-schemas.json # export tools without starting a session
```

Every launch starts a fresh session.

## Configuration

Frances reads `config.toml` from a layered set of sources, **later layers
overriding earlier ones**:

1. XDG system config dirs (`XDG_CONFIG_DIRS`, default `/etc/xdg/frances/`).
2. XDG user config dir — `~/.config/frances/config.toml`.
3. `FRANCES__*` environment variables.
4. Per-session database rows.

Every TOML file is optional; running with no config file present is supported.

A complete `~/.config/frances/config.toml` looks like this:

```toml
[model_providers.codex]
kind = "openai-responses"
name = "Codex"
base_url = "https://chatgpt.com/backend-api/codex/"
auth = { codex = true }

[model_providers.codex.http_headers]
"OpenAI-Beta" = "responses=experimental"
originator = "codex_cli_rs"

[model_providers.zai]
kind = "zai"
name = "Z-AI"
base_url = "https://example.com/api/coding/paas/v4"
auth = { file = "/home/jono/.config/frances/zai.txt" }

[model_providers.deepseek]
kind = "deepseek"
name = "Deepseek"
base_url = "https://api.deepseek.com"
auth = { file = "/home/jono/.config/frances/ds.txt" }

[models.default]
model_provider = "codex"
id = "gpt-5.5"
effort = 50
effort_tiers = "openai"

[models.cheap]
model_provider = "codex"
id = "gpt-5.4-mini"

```

### Providers (`[model_providers.<id>]`)

The table key (`codex`, `zai`, `deepseek`) is the provider id referenced by
`[models.*].model_provider`. Each provider supports:

| Field                    | Required | Notes                                                     |
| ------------------------ | -------- | --------------------------------------------------------- |
| `kind`                   | yes      | Adapter selector — see below.                             |
| `base_url`               | yes      | Provider API base URL.                                    |
| `auth`                   | yes      | Auth method — see below.                                  |
| `name`                   | no       | Human-facing display name.                                |
| `http_headers`           | no       | Extra request headers (values support env-var expansion). |
| `query_params`           | no       | Extra query params (values support env-var expansion).    |
| `supports_websockets`    | no       | Default `false`.                                          |
| `request_max_retries`    | no       | Default `4`.                                              |
| `stream_max_retries`     | no       | Default `5`.                                              |
| `stream_idle_timeout_ms` | no       | Default `300000`.                                         |

`kind` is validated at provider-build time (`parse_kind` in
`crates/frances-llm/src/providers/genai/kinds.rs`). Accepted values:

```
openai-chat   openai-responses   anthropic   gemini   openrouter
zai           deepseek           moonshot    ollama   groq
xai           together           fireworks
```

Anything else is a config error.

### Auth (`auth = { ... }`)

The `auth` field deserializes into the untagged `AuthMethod` enum
(`crates/frances-models-llm/src/config.rs`). serde walks the variants
top-to-bottom and picks the first whose required fields are present, so each
variant is distinguished purely by its shape:

```toml
# Codex / ChatGPT-subscription auth. Access token is read from auth.json and
# refreshed on demand. `codex = true` is required; `codex = false` is rejected.
auth = { codex = true }
auth = { codex = true, codex_home = "/home/jono/.codex" }   # override credential dir

# Read a bearer token from a file (trimmed).
auth = { file = "/home/jono/.config/frances/zai.txt" }

# Read a bearer token from an environment variable.
auth = { env_key = "DEEPSEEK_API_KEY" }
auth = { env_key = "DEEPSEEK_API_KEY", env_key_instructions = "Get one at https://..." }

# Inline literal token.
auth = { token = "sk-..." }

# Run a command to mint a token (not yet implemented at request time).
auth = { command = { command = "get-token", args = ["--json"], cwd = "/some/dir", refresh_interval_ms = 3600000, timeout_ms = 5000 } }
```

#### Where `AuthMethod` is used

Auth resolution happens in exactly one place: `resolve_auth` in
[`crates/frances-llm/src/providers/genai/request_plan.rs`](crates/frances-llm/src/providers/genai/request_plan.rs).
It is called from `RequestPlan::build` and exhaustively matches every variant:

| Variant   | Behaviour                                                                                    |
| --------- | -------------------------------------------------------------------------------------------- |
| `EnvKey`  | Reads the named env var; errors `MissingEnvVar` (surfacing `env_key_instructions`) if unset. |
| `Token`   | Uses the literal token as-is.                                                                |
| `File`    | Reads and trims the file; errors `ReadAuthFile` on IO failure.                               |
| `Codex`   | Resolves via `codex_auth`, returning an access token plus a `ChatGPT-Account-ID` header.     |
| `Command` | Returns `AuthCommandUnimplemented` — defined but not yet wired up.                           |

`AuthMethod` is defined in `frances-models-llm` and re-exported from
`frances-models-llm` and `frances-llm`. Outside of tests, `resolve_auth` is its
only reader.

### Models (`[models.<name>]`)

Each model binds a `model_provider` (a provider id) to a model `id`. `default`
and `cheap` are the conventional names. `effort` is an optional normalized
integer percentage from 0 through 100. `effort_tiers` maps it onto provider
labels and accepts either the `"openai"` preset or an explicit ascending array,
for example `["off", "low", "high"]`. A chat-session override takes precedence
over the model default; without either value no effort is sent.

### Native harness

The Rust host runs ordinary chat and tools directly. No workflow script or
workflow selection is required. Shell commands use the existing permission UI;
file edits use the anchor engine and worker filesystem. See the
[session runtime](docs/arch/session-runtime.md) for loop and persistence details.

Run `just check-tool-schemas` to export all built-in tools (including the permission
judge) and check their schemas using the pinned npm `tool-schema` package. The
script installs its locked dependency and checks each tool in the strict or
non-strict mode selected by the provider. It also checks for missing strict-mode
types and array items, which the package currently misses. This is a local lint,
not a guarantee that every provider will accept a schema.

To check an existing export, run `node opt/check-tool-schemas.mjs tool-schemas.json`.
The Nix development shell includes Node.js and npm.

Load additional MCP definitions explicitly with `frances --mcp mcp.jsonc`; use `--mcp-server NAME`
to select one. See [MCP configuration](docs/arch/mcp.md) for JSONC fields and execution placement.
