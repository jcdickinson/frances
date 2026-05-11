// `whatwg:dom` — minimal DOM Standard surface.
//
// QuickJS ships no DOM. We add bits here only as the host runtime or
// the polyfills we ship actually need them; this is NOT a general-
// purpose DOM polyfill. Today: `DOMException`, used by
// `whatwg:abortcontroller` for the default abort reason.
//
// See `docs/js/whatwg.md` for the policy on what belongs here.

class DOMException extends Error {
  constructor(message, name = "Error") {
    super(message);
    this.name = name;
  }
}

export { DOMException };
