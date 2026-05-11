#!/usr/bin/env sh
# Refresh vendored sources for the `whatwg:*` virtual modules.
#
# Run from anywhere; paths resolve relative to this script.
#
# web-streams-polyfill v4 only ships its ES-module build as
# `dist/ponyfill.mjs` (named exports, no globalThis mutation).
# `dist/polyfill.js` exists but is CJS-only — there is no
# `dist/polyfill.mjs`. A ponyfill is what we want anyway:
# the `whatwg:web-streams` virtual module re-exports named
# bindings rather than scribbling onto globalThis.
#
# AbortController is hand-rolled (see abortcontroller.mjs).
# abortcontroller-polyfill ships its ESM via a path that
# does not load as a standalone module.

set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
WEB_STREAMS_VERSION=4.2.0

curl -fsSL \
    "https://unpkg.com/web-streams-polyfill@${WEB_STREAMS_VERSION}/dist/ponyfill.mjs" \
    -o "$DIR/web-streams.mjs"

printf 'updated web-streams.mjs -> web-streams-polyfill@%s\n' "$WEB_STREAMS_VERSION"
