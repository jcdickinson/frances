# Frances

Frances is an agentic coding tool. The `frances` binary is a single-process TUI:
it identifies the controlling TTY, resolves (or creates) a per-TTY session, opens
a per-session [turso](https://github.com/tursodatabase/turso) database, constructs
an in-process `SessionRuntime`, and runs the TUI directly against it. LLM
completions stream through a configurable set of model providers.

> [!WARNING]
> **This project is almost entirely coded by an LLM.** Treat the code, comments,
> and this README accordingly: assume nothing has been hand-audited unless you
> verify it yourself. Frances has also never shipped — there is no released
> version and no backward-compatibility burden, so schemas, formats, and APIs
> change freely.

## TL;DR — get up and running

```bash
# 1. Get the toolchain: either the dev shell (adds rust-analyzer, jq, cargo machete, ...)
nix develop                    # or: rustup install 1.95.0

# 2. Build, then run the installer. It asks a few questions, writes a starter
#    config.toml, and drops the `main` workflow into ~/.config/frances.
cargo run -p frances -- install            # copies the workflow into the config dir
cargo run -p frances -- install --local    # instead points the config at the in-repo workflow

# 3. Run it.
./target/debug/frances        # opens the TUI for this terminal's session
```

`install` always (re)installs the `main` workflow; it only runs the
questionnaire when `config.toml` doesn't already exist, so re-running it
refreshes the workflow without clobbering a config you've since edited. The
questionnaire offers your Codex (ChatGPT) login, or any other provider — for
which it writes the token you paste to `~/.config/frances/<provider>.txt`.

Once the TUI is up, type `/main` to kick off the `main` workflow.

If you'd rather write the config by hand, a minimum viable `config.toml`:

```toml
[model_providers.deepseek]
kind = "deepseek"
base_url = "https://api.deepseek.com"
auth = { file = "/home/you/.config/frances/ds.txt" }

[models.default]
model_provider = "deepseek"
id = "deepseek-chat"

# A workflow to drive the session. Point `file` at the one shipped in this repo.
default_workflow = "main"

[workflows.main]
id = "e3c5d9f6-141b-4cf8-b6ad-41e5a9cdee43"
file = "/home/you/Code/frances/assets/workflows/main.ts"
```

`frances new` later unlinks this terminal's session so the next run starts fresh.

## The name

Frances is named for two early UNIVAC programmers:

- **Frances E. Holberton** (1917–2001) — one of the six original ENIAC
  programmers, who went on to work on UNIVAC. She wrote the C-10 instruction set
  and the *Sort-Merge Generator*, an early example of a program that writes
  programs — a fitting namesake for an agentic coding tool.
- **Frances ("Betty") Morello** — a UNIVAC programmer of the same era.

## Workspace layout

The interesting crates live under `crates/`:

- **`frances`** — the binary. TUI, TTY identification, and `main.rs` wiring the
  runtime to the TUI.
- **`frances-session`** — session runtime: per-session DB handle, workflow stack,
  history, scrollback persistence, anchor store, the LLM session provider, and the
  events channel into the TUI.
- **`frances-workflow`** — JS-driven workflow runtime (rquickjs) that drives chat
  sessions and tool calls.
- **`frances-llm`** / **`frances-models-llm`** — provider configuration, auth
  resolution, and the genai-backed request plan.
- **`frances-edit`** — anchor-based file edit engine. Filesystem-agnostic.
- **`frances-anchors`** — anchor word dictionary plus line hashing and
  word↔index encoding.

Architecture docs live in [`docs/arch/`](docs/arch/).

## Building

The workspace pins a single Rust toolchain in `rust-toolchain.toml`
(1.95.0, edition 2024).

```bash
cargo build                  # build everything
cargo build -p frances       # just the binary
cargo nextest                # run all tests
cargo fmt --all
cargo clippy --all-targets
nix build                    # reproducible build via flake.nix
```

A `nix develop` dev shell provides the toolchain plus `rust-analyzer`, `jq`,
`python3`, and `cargo machete` (the unused-dependency check).

## Running

```bash
frances        # open the TUI for the current TTY's session (creating one if none)
frances new    # unlink the current TTY's session so the next run starts fresh
```

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

[models.cheap]
model_provider = "codex"
id = "gpt-5.4-mini"

[workflows.main]
id = "e3c5d9f6-141b-4cf8-b6ad-41e5a9cdee43"
file = "/home/jono/Code/frances/assets/workflows/main.ts"
```

### Providers (`[model_providers.<id>]`)

The table key (`codex`, `zai`, `deepseek`) is the provider id referenced by
`[models.*].model_provider`. Each provider supports:

| Field                    | Required | Notes                                                          |
| ------------------------ | -------- | -------------------------------------------------------------- |
| `kind`                   | yes      | Adapter selector — see below.                                  |
| `base_url`               | yes      | Provider API base URL.                                         |
| `auth`                   | yes      | Auth method — see below.                                       |
| `name`                   | no       | Human-facing display name.                                     |
| `http_headers`           | no       | Extra request headers (values support env-var expansion).      |
| `query_params`           | no       | Extra query params (values support env-var expansion).         |
| `supports_websockets`    | no       | Default `false`.                                               |
| `request_max_retries`    | no       | Default `4`.                                                   |
| `stream_max_retries`     | no       | Default `5`.                                                   |
| `stream_idle_timeout_ms` | no       | Default `300000`.                                              |

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

| Variant        | Behaviour                                                                                   |
| -------------- | ------------------------------------------------------------------------------------------- |
| `EnvKey`       | Reads the named env var; errors `MissingEnvVar` (surfacing `env_key_instructions`) if unset. |
| `Token`        | Uses the literal token as-is.                                                                |
| `File`         | Reads and trims the file; errors `ReadAuthFile` on IO failure.                               |
| `Codex`        | Resolves via `codex_auth`, returning an access token plus a `ChatGPT-Account-ID` header.     |
| `Command`      | Returns `AuthCommandUnimplemented` — defined but not yet wired up.                           |

`AuthMethod` is defined in `frances-models-llm` and re-exported from
`frances-models-llm` and `frances-llm`. Outside of tests, `resolve_auth` is its
only reader.

### Models (`[models.<name>]`)

Each model binds a `model_provider` (a provider id) to a model `id`. `default`
and `cheap` are the conventional names.

### Workflows (`[workflows.<name>]`)

A workflow is the JS/TS script that drives a chat session and its tool calls.
The table key (`main`, `plan`) is the name the rest of the config refers to.

| Field        | Required | Notes                                                                              |
| ------------ | -------- | ---------------------------------------------------------------------------------- |
| `id`         | yes      | Stable UUID. The workflow owns a chunk of the per-session DB schema under this id.  |
| `file`       | yes      | Absolute path to the workflow script that gets loaded and run.                     |
| `migrations` | no       | SQL migration files in apply order, resolved **relative to `file`'s directory**.   |

So in the example above, `file = ".../assets/workflows/main.ts"` is the script
the `main` workflow executes, and its `id` is the entity that namespaces any DB
rows that workflow persists. If `main.ts` needed schema, you would co-locate
`0001_init.sql` next to it and list `migrations = ["0001_init.sql"]`.

Which workflow runs at boot is chosen by the top-level `default_workflow` key,
which names one of the `[workflows.*]` entries:

```toml
default_workflow = "main"
```

On a fresh session the runtime seats `default_workflow`; on an existing session
it restores the persisted workflow stack instead. If `default_workflow` is unset
(as in the example config), the session starts with an empty stack.
