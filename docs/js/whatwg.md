# `whatwg:*` modules

Workflow scripts run on QuickJS, which ships only ECMAScript. Anything
from the WHATWG / WebIDL world — DOM, Fetch, Streams, AbortController,
etc. — isn't there. We expose the bits we want under a `whatwg:` virtual
module namespace, imported the normal ES way:

```js
import { ReadableStream } from "whatwg:web-streams";
import { AbortController } from "whatwg:abortcontroller";
import { DOMException } from "whatwg:dom";
```

Sources live at the workspace root under `modules/whatwg/` and are
embedded into the binary via `include_str!`. Where a usable upstream
polyfill exists we vendor it; `modules/whatwg/update.sh` refreshes those
from unpkg. Where the upstream is unusable (CJS-only, busted ESM build,
EventTarget-dependent) we hand-roll instead.

## `whatwg:dom` — DOM on a what-we-need basis

**Don't treat `whatwg:dom` as a DOM polyfill.** It's a deliberately
narrow grab-bag of things from the DOM Standard that our runtime or our
other polyfills need to function. Today that's just `DOMException`,
which `whatwg:abortcontroller` uses for the default abort reason.

Rules for adding to it:

- It has to come from the [DOM Standard](https://dom.spec.whatwg.org/).
- Something the runtime, a vendored polyfill, or an in-tree workflow
  actually uses today. We aren't speculatively building out a DOM.
- Match the spec shape closely enough that real-world JS that targets
  it will work. Half-implementing methods is worse than not having them.
- If the implementation gets non-trivial, consider whether the runtime
  feature pulling it in is worth it.

Things explicitly out of scope: `Node`, `Element`, `Document`, the
event-loop / `EventTarget` machinery, `HTMLCollection`, MutationObserver,
URL/URLSearchParams (those are WHATWG URL — separate module if we ever
need them), structured clone.

When something graduates from "nice to have" to "needed", add it here
and update this list. Don't pre-build.

## Current modules

| Module                  | Origin                                                | Notes                                                                                  |
| ----------------------- | ----------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `whatwg:web-streams`    | `web-streams-polyfill@4` `dist/ponyfill.mjs` (vendored) | Ponyfill build (named exports, no globalThis mutation). Refresh with `update.sh`.      |
| `whatwg:abortcontroller`| hand-rolled                                           | `abortcontroller-polyfill`'s ESM doesn't load standalone. No EventTarget; no `timeout`.|
| `whatwg:dom`            | hand-rolled                                           | Just `DOMException` today. See policy above before adding.                             |
