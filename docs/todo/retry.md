# Honour HTTP `Retry-After` on transient provider retries

The transparent transient-failure retry (TFR) in `frances-llm`'s genai provider
(`crates/frances-llm/src/providers/genai/mod.rs`) retries pre-output failures
with plain exponential backoff. It does **not** read the server's `Retry-After`
header. For a real 429 from a rate-limiting provider, honouring `Retry-After`
would back off by the amount the server actually asked for instead of guessing.

## Why it was deferred: the header isn't uniformly reachable

`genai::Error` (genai 0.6.0-beta.20, `src/error.rs`) carries the status but the
headers are only present on one path:

- `HttpError { status: StatusCode, canonical_reason, body }` — status, **no
  headers**.
- `WebStream { model_iden, cause: String, error: BoxError }` — this is the
  variant a mid-flight streaming 500 surfaces as (the one that wedged the
  session). Status lives only inside the `cause` string; **no headers, no
  structured status.**
- `WebModelCall { webc_error } | WebAdapterCall { webc_error }` wrap
  `genai::webc::Error`, whose `ResponseFailedStatus { status, body, headers:
  Box<HeaderMap> }` **does** carry the headers — so `Retry-After` is reachable
  here.

So `Retry-After` is available for the webc / connection-establishment path but
**discarded on the streaming path**, which is exactly where our flakiness shows
up. Honouring it only on the webc path would be inconsistent; honouring it on
the streaming path needs an upstream genai change (preserve headers on
`WebStream`) or dropping below genai to a custom streaming client.

Do **not** parse the status/retry-after back out of the `cause`/`body` strings —
that's the stringly-typed-error trap the project bans.

## When to pick this up

When a provider actually rate-limits us (sustained 429s) and blind backoff
either hammers too hard or waits too long. Until then, exponential backoff is
enough — the failures we've seen are transient 5xx / dropped streams, not rate
limits.
