# Frances — UI glossary

Living document. Terms are resolved as they come up; do not treat this file as
a spec.

- **Section** — a workflow-emitted, lifecycle-bounded unit of transcript
  content. Kinds include Markdown, ShellOutput, Reasoning, ToolUse, Diff, Json,
  and Error. Sections are persisted and replayed by the session runtime.
- **Section append** — opens a previously unseen section or adds text and new
  metadata to an existing section.
- **Section close** — seals a live section. A truncated close means replay found
  a section that was still open when its workflow was dehydrated.
- **Scrollback replay** — a reset/sections/end burst sent when the frontend
  connects or the active workflow changes. The Svelte store applies it through
  the same reducer as live section events.
- **Surface command** — ephemeral workflow-owned chrome: footer status and
  window title.
- **Frontend-ready handshake** — the frontend installs its Tauri event listener
  before asking Rust to start forwarding buffered runtime events. This prevents
  initial replay frames from being lost while the webview loads.
- **Workspace** — what a launch opens: a directory (implicit single-dir
  workspace) or a workspace file listing several dirs. Canonicalized before
  the launcher detaches; every launch creates a fresh session recording its
  workspace source.
