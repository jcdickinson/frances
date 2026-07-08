#!/usr/bin/env sh
# Refresh vendored sources for embedded workflow assets.
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
# AbortController is hand-rolled (see abortcontroller.js).
# abortcontroller-polyfill ships its ESM via a path that
# does not load as a standalone module.

set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
WEB_STREAMS_VERSION=4.2.0
MUSTACHE_VERSION=4.2.0

curl -fsSL \
    "https://unpkg.com/web-streams-polyfill@${WEB_STREAMS_VERSION}/dist/ponyfill.mjs" \
    -o "$DIR/whatwg/web-streams.js"

printf 'updated web-streams.js -> web-streams-polyfill@%s\n' "$WEB_STREAMS_VERSION"

mkdir -p "$DIR/vendor"

curl -fsSL \
    "https://unpkg.com/mustache@${MUSTACHE_VERSION}/mustache.mjs" \
    -o "$DIR/vendor/mustache.js"

printf 'updated vendor/mustache.js -> mustache@%s\n' "$MUSTACHE_VERSION"
