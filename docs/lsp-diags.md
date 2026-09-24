# LSP diagnostic synchronization

Design notes from the September 2026 discussion. This describes a proposed
integration, not functionality already implemented in Frances.

The goal is useful, timely diagnostic feedback with explicit freshness tracking.
LSP supports concurrent editing. It does not provide a universal barrier saying
that every diagnostic producer has analyzed a particular workspace snapshot.
That limitation should shape the integration, not prevent us from using it.

## Preferred flow

Use document pull diagnostics when the server supports them:

1. Synchronize known changes through `didOpen` and `didChange`.
2. Record the local workspace generation and document versions for the request.
3. Request `textDocument/diagnostic` for relevant documents.
4. Await the report.
5. If relevant inputs changed while the request ran, discard it and schedule a
   replacement after the edit batch. Otherwise, consume the report.

The protocol explicitly asks document diagnostic providers to compute results
for the currently synchronized document. Responses do not echo its version;
associate requests with versions locally. `resultId` is an opaque cache token,
not a document revision. Handle both full and unchanged reports, cancellation,
and server requests to refresh diagnostics.

`workspace/diagnostic` is different: it can be long-running and is explicitly
not bound to one workspace or document state. It is not a snapshot barrier.
Negotiate capabilities instead of assuming either pull method is available.

Source: [LSP pull diagnostics][pull].

## Change tracking belongs in the workspace integration

Individual filesystem tools do not need to understand LSP. Keep the edit engine
filesystem-agnostic. A shared workspace integration observes writes and owns
language-server synchronization:

```text
File tools -----------+
                      +--> workspace change tracking --> LSP synchronization
Filesystem watcher ---+
```

Frances performs I/O through a remote worker. Observe filesystem changes there,
where the files actually live. Forward change information to whichever component
owns the language-server connection.

Successful writes through Frances should update tracking directly. Watchers cover
shell commands, formatters, users, and other agents. Compare contents with the
tracked text so duplicate watcher events do not create duplicate revisions.
Do not require every shell command to enumerate the files it changes.

For a changed client-managed document, increment its version and send the new
contents or incremental changes. Send save notifications for saved changes when
requested by the server. Invalidate previous diagnostic freshness immediately.

Start with a conservative generation counter covering relevant workspace changes.
Changing a trait can invalidate diagnostics in an unchanged implementation file.
The generation is local bookkeeping: never stamp an unsolicited report with the
current generation and assume the server analyzed it.

Watchers have delivery delays. A generation only covers observed changes; it is
not proof that no external writer changed disk. Batch related edits and avoid
repeatedly checking intentionally incomplete intermediate states. A timeout,
cancellation, or missing report means pending or unavailable, never clean.

## Document ownership and versions

`didOpen` transfers authority over a document's contents to the client. “Open”
means client-managed, not necessarily visible in an editor. `didChange` updates
that text, with a monotonically increasing version, including for undo/redo.
`didClose` returns authority to the document's backing storage.

Opening a document once and then modifying only its disk file leaves the server
with the old client-supplied text. Filesystem notifications do not substitute for
`didChange` on open documents.

Closed files have no client-managed LSP version to compare. Frances may track
their hashes, but those hashes do not automatically appear in server reports.
Opening a file after receiving a diagnostic does not validate that report
retroactively. Open and synchronize it before requesting fresh analysis.

Sources: [document ownership][open], [closing documents][close],
[document changes][change], [version identifiers][versions].

## Push diagnostics

Frances can choose which push reports to expose to the agent. There is no universal
unsubscribe switch: advertising pull support does not necessarily disable pushes,
and omitting `publishDiagnostics` capabilities is not an unsubscribe mechanism.
Check server behavior before dropping pushes; some producers may supply findings
only through that channel.

For incoming `publishDiagnostics`:

| Version | Treatment |
| --- | --- |
| Matches current document | Accept as describing that document version. |
| Older | Discard as superseded. |
| Missing | Freshness unknown. Do not assign a current version. |
| Newer | Investigate synchronization bookkeeping. |

An accepted push replaces that server's previous report for the document. An empty
report clears it. Keep push and pull provenance distinct rather than blindly
merging old pushes into fresh pulls.

A matching version identifies the text the server says it diagnosed. It does
not establish that every dependency or background compiler input was current.

Source: [LSP push diagnostics][push].

## Applying code fixes

A versioned `TextDocumentEdit` supplies an expected document version. Check that
precondition and apply the edit in one serialized operation. Checking, yielding,
and then writing still permits a race. All cooperating writers must use the same
coordination point.

`WorkspaceEdit.documentChanges` supports versioned edits; the older `changes`
mapping does not. A null version also provides no numeric precondition. For
unversioned edits, retain the request's contents as a local precondition; if the
necessary baseline is unavailable, obtain it and recompute rather than guessing.

For fixes spanning files, validate every target before applying anything and use
transactional application where available. LSP negotiates failure handling;
atomic multi-file application is not universal. Arbitrary external disk writers
remain outside in-process coordination.

The edit engine needs content preconditions, not knowledge of LSP. The LSP adapter
translates server edits into those preconditions. A matching target file alone
does not prove semantic freshness if a dependency changed during computation.

Sources: [text document edits][edits], [workspace edits][workspace-edits].

## rust-analyzer and compiler checks

rust-analyzer's native analysis and background Cargo checks are separate producers.
A completed document pull is not a promise that a Cargo check completed against
the same inputs. Track compiler validation separately.

At meaningful validation points, the remote worker can run a scoped Cargo check,
record its starting generation, and collect structured diagnostics. If relevant
inputs change during the run, mark the result superseded and schedule another.
A stable snapshot or coordinated writers are needed for stronger guarantees.

rust-analyzer exposes `runFlycheck`, `cancelFlycheck`, and `clearFlycheck`, but
`runFlycheck` is a notification, not a request returning a completed report.
`experimental/serverStatus` includes `quiescent`; its documented purpose is
status reporting, not acknowledgement that all diagnostics for a particular
workspace generation have been delivered. Idle status can itself arrive after
the client has made another edit. `viewFileText` can help diagnose synchronization
problems by showing the server's view of a document.

Use the running server's advertised capabilities. Server versions can differ in
which diagnostic providers they expose.

Sources: [rust-analyzer diagnostics][ra-diags], [LSP extensions][ra-extensions].

[pull]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/pullDiagnostics.md
[push]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/language/publishDiagnostics.md
[open]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/textDocument/didOpen.md
[close]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/textDocument/didClose.md
[change]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/textDocument/didChange.md
[versions]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/types/versionedTextDocumentIdentifier.md
[edits]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/types/textDocumentEdit.md
[workspace-edits]: https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.17/types/workspaceEdit.md
[ra-diags]: https://rust-analyzer.github.io/book/diagnostics.html
[ra-extensions]: https://rust-analyzer.github.io/book/contributing/lsp-extensions.html
