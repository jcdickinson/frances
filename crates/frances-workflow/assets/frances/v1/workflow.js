// `frances:v1/workflow` — re-exports `exit` + `setStatus` from the
// install-time stash and layers `setTitle`/`getTitle` over the host's
// `_setTitle` primitive. The stash is deleted from globalThis after all
// modules evaluate, so the destructured bindings end up captured in this
// module's local scope only.
//
// `setStatus(text | null)` drives the TUI footer busy indicator:
// a string shows the text with a spinner, `null` hides it.
//
// `setTitle(text | null)` sets or clears the session title, which the
// host persists across restarts. `getTitle()` returns the current title
// (or `null`): the persisted value at boot, updated locally on every
// `setTitle` — the workflow is the only writer, so no host round-trip.

export const { exit, setStatus } = globalThis.__frances_v1_stash__;

const { _setTitle, _initialTitle } = globalThis.__frances_v1_stash__;

let currentTitle = _initialTitle ?? null;

export function setTitle(text) {
  currentTitle = text ?? null;
  _setTitle(currentTitle);
}

export function getTitle() {
  return currentTitle;
}
