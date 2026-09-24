# Model Content + Hooks Protocol: Databases

Status: draft proposal, not implemented. Extension version: 1.

This companion to the [core protocol](model-content-hooks-protocol.md) proposes
`frances/db`: persistent, host-provided databases for MCP servers. The server owns
its records and schema; the host provides storage, isolation, and transactions.
Using this service is optional. A server can still use its own database, and a
host MUST NOT require application state to be stored through this extension.

The starting point is [IndexedDB](https://www.w3.org/TR/IndexedDB/): named
databases containing object stores, primary keys, secondary indexes, and atomic
transactions. This is an RPC design inspired by that data model, not an
implementation of the browser API.

All methods below are host operations inside the core draft's
`frances/hostInputRequired` continuation, not standalone server-to-host RPCs.
This extension requires MCP 2026-07-28 and `frances/session`. Older MCP revisions
can provide classic MCP features only.
Fields use camelCase and methods use `frances/db/`. MUST, SHOULD, and MAY have the
same meaning as in the core draft. Shapes are illustrative; complete JSON Schemas
and numeric error codes remain to be specified.

## Design choices

- A transaction is one request containing an ordered list of operations. There
  is no remote `begin`, per-record callback, or separate `commit` request.
- Reads and writes accept arrays. Range reads return bounded pages, not one
  cursor step per network round trip.
- Read/compute/write uses optimistic concurrency. Application code runs on the
  server without holding a host database transaction open.
- Mutations carry durable request IDs. Retrying after a lost response returns
  the recorded outcome instead of executing the mutation again.
- Values are JSON, keys are explicit, and indexes are declarative. SQL, uploaded
  functions, and a general expression language are not part of this proposal.

The browser analogy is a server's private origin storage. This does not expose
the host's transcript tables or a connection to an arbitrary external database.

## Negotiation and isolation

Use the core protocol's version-intersection rule. Both sides advertise
`frances/db`; the host additionally advertises supported scopes and limits:

```json
{
  "capabilities": {
    "extensions": {
      "frances/session": { "versions": [1] },
      "frances/db": {
        "versions": [1],
        "scopes": ["session", "workspace"],
        "limits": {
          "maxRequestBytes": 1048576,
          "maxResponseBytes": 1048576,
          "maxOperations": 256,
          "maxRecordsPerOperation": 1000,
          "maxRecordBytes": 262144,
          "maxPageRecords": 1000,
          "maxTransactionMs": 5000
        }
      }
    }
  }
}
```

These numbers are examples, not defaults. Hosts MUST advertise positive limits.
Byte limits count compact UTF-8 JSON encodings of params, results, or individual
`{key, value}` records as applicable. Structural validation and request-size
checks happen before execution. Runtime budget exhaustion aborts a transaction;
it never commits a prefix. Hosts may enforce storage and receipt quotas and
report quota exhaustion explicitly.

| Scope | Binding and lifetime |
| --- | --- |
| `session` | The current host user session; survives context replacement, disconnect, and process restart; deleted only by an explicit deletion action |
| `workspace` | The host's bound workspace; shared across that workspace's sessions for the same server identity; retained until explicit deletion |

Session support is required; workspace support is optional. There is no implicit
fallback between scopes. The host resolves the workspace, including multi-root
identity; callers cannot supply a path to select a different workspace.

The storage namespace is `(host security principal, server identity, scope
binding)`. Database names are private within that namespace. Server identity is
assigned by the host from its trusted connection configuration, not taken from
the server's self-reported name, an RPC parameter, or a tool argument. Distinct
configured identities cannot read one another's databases. Reconnection must
restore the same binding before storage requests are accepted.

An application session ID and a model context ID are not database names. A fresh context
does not clear storage. A fresh connection does not grant access to arbitrary
previous sessions. Each operation inherits the application session and trusted
server identity of its originating client RPC; an embedded operation cannot
choose another binding. The host MUST support servicing database requests while
awaiting a server's tool, hook, or UI response; otherwise a controller persisting
its state would deadlock.

The core draft's continuation contract works identically over HTTP and stdio.
The server yields a host-operation request, and the host returns its result in
`_meta["frances/session"].continuation.hostResponses` on the next client RPC round.
The server commits no dependent workflow result until it receives the storage
outcome. The operation's durable `requestId` stays the same across lost responses,
new JSON-RPC IDs, and continuation rounds. Transport errors never imply rollback.

The application session binding must exist before database operations are
accepted. Session creation itself cannot depend on these host operations;
a server using host storage must retain its session registry and creation/deletion
receipts independently, and may store the workflow contents through `frances/db`.

## Data model

A database has a name, an opaque stable `databaseId`, a positive integer
`schemaVersion`, and an opaque `revision`. Reopening returns the same ID;
deleting and recreating the name allocates a different ID. IDs are checked
against the caller's namespace on every request and are not bearer credentials.

An object store maps unique primary keys to JSON values, including JSON null.
Keys are supplied separately from values. There are no key generators: callers
can allocate IDs locally and use them throughout a transaction without waiting
for an insert response.

A key is a string, a safe integer in the inclusive range
`-(2^53 - 1)` through `2^53 - 1`, or a nonempty array of strings and safe integers.
Nested arrays are invalid. Numbers sort before strings, strings before arrays.
Numbers sort numerically; strings sort lexicographically by Unicode scalar value
without normalization or locale collation. Arrays sort lexicographically by
their elements, with a shorter equal prefix first. Negative zero equals zero.
Strings must contain valid Unicode scalar values. This intentionally defines a
smaller key space than browser IndexedDB.

Values must be ordinary JSON with unique object member names and finite numbers.
Portable exact integers use the safe range above; applications encode larger
integers as strings. Binary data can be encoded by an application within record
limits; native blobs and JavaScript structured cloning are not wire types.

An index has a name, one or more `paths`, and a `unique` boolean. Each path is a
JSON Pointer into the record value. One path extracts a key directly; multiple
paths produce a compound array key in path order, with scalar components only.
If a path is absent or its value cannot form a valid key, the record has no entry
in that index. Missing indexed fields do not invalidate the record. There is no
array expansion (`multiEntry`) in this proposal.

Indexes are maintained atomically with records. A unique index rejects a write
that would introduce a duplicate index key. Reads through an index order by
`(indexKey, primaryKey)` so duplicate index keys have deterministic ordering.

## Opening and changing a schema

`frances/db/open` accepts `scope`, `name`, and optionally `create`. Without
`create`, a missing database is an error. With `create`, it also requires a
`requestId` and creates the database atomically if absent:

```json
{
  "scope": "session",
  "name": "planner",
  "requestId": "create-planner-1",
  "create": {
    "schemaVersion": 1,
    "stores": [
      { "name": "state", "indexes": [] },
      {
        "name": "steps",
        "indexes": [
          { "name": "byStatus", "paths": ["/status"], "unique": false },
          { "name": "byPosition", "paths": ["/position"], "unique": true }
        ]
      }
    ]
  }
}
```

The result contains `databaseId`, `schemaVersion`, `revision`, and the full
`stores` schema. Opening an existing database with `create` succeeds only if its
schema and version match the supplied definition; otherwise it returns
`schemaMismatch`. Store and index names must be nonempty and unique in their
respective scopes. New databases start at schema version 1. Open is normally one
startup round trip, not a prerequisite for every transaction. There are no
remote connection handles to close.

`frances/db/schema` accepts `databaseId`, `requestId`, `expectedSchemaVersion`,
`expectedRevision`, and ordered `changes`. Changes are `createStore`,
`deleteStore`, `createIndex`, and `deleteIndex`, carrying the relevant store or
index definition/name. Both expectations are required. Success applies all
changes atomically, increments schema version by one, assigns a fresh revision,
and returns the full schema as for open. Existing record data is indexed before
success; duplicate values in a new unique index abort the entire change.

Deleting a store explicitly deletes its records. Changing an index means deleting
and recreating it. Structural changes serialize with ordinary transactions;
callers holding an earlier schema version receive `schemaMismatch` on their next
transaction. There are no version-change callbacks waiting for other clients to
close connections.

Application-level data transformations use ordinary transactions. Large changes
can populate a new store in bounded batches and switch the application's active
store reference in one transaction. The protocol does not run migration scripts.
Whether atomic structural changes plus data operations are needed in the same
request is a design question below.

## Transactions

`frances/db/transaction` accepts `databaseId`, `schemaVersion`, `mode`, and
`operations`. Mode is `readonly` or `readwrite`. Readwrite requires `requestId`;
either mode may include `expectedRevision`.

All operations execute in order in one serializable transaction across the
referenced stores in that database. Reads see preceding writes. A failed
operation aborts the whole transaction. There are no partial-success batches,
cross-database transactions, or server callbacks during execution.

Every committed readwrite transaction assigns a new database revision, including
a write that leaves the same values. Schema changes do likewise. Revisions are
never reused within a database incarnation. Readonly transactions do not change
the revision. Expected revision is compared before any operation executes and
under the same concurrency control as the transaction itself.

### Operations

Each operation is a tagged object with `type` and `store`:

| Type | Fields beyond `type` and `store` | Result |
| --- | --- | --- |
| `get` | `keys` array | One entry per input key, preserving order: `{found: false}` or `{found: true, value: ...}` |
| `add` | `records` array of `{key, value}` | `{written: N}`; any existing primary key fails the transaction |
| `put` | `records` array of `{key, value}` | `{written: N}`; inserts or replaces complete values |
| `delete` | `keys` array | `{deleted: N}`; absent keys are harmless |
| `clear` | None | `{deleted: N}` |
| `count` | Optional `index` and `range` | `{count: N}` |
| `scan` | Optional `index`, `range`, `after`; required `direction`, `limit`, `includeValues` | A bounded page described below |

Writes require readwrite mode. Duplicate primary keys within one operation are
invalid; writing a key again in a later operation is allowed. `written` counts
input records and `deleted` counts records actually removed. Count is exact or
fails on execution limits; it does not return an estimate. An index count counts
index entries, including entries sharing an index key.

A range is either `{only: key}` or an object with optional `lower` and `upper`
bounds. Each bound is `{key: ..., inclusive: true|false}`. Omission of `range`
selects all keys. Invalid or reversed bounds are rejected. Ranges apply to index
keys when `index` is present and otherwise to primary keys.

The result is `{revision, results}`, with one result per operation in input
order. Revision identifies the snapshot for a readonly transaction and the
committed state for readwrite. The host checks response limits before commit;
an oversized result aborts instead of committing an unreportable mutation.

### Example: finish a step atomically

The server has read revision `r17`, performed its review, and computed the new
state. One host operation updates the step and workflow together. The following object
is the value of a `hostRequests` entry named `finish-step`:

```json
{
  "method": "frances/db/transaction",
  "params": {
    "databaseId": "db-7",
    "schemaVersion": 1,
    "mode": "readwrite",
    "requestId": "finish-step-3-at-r17",
    "expectedRevision": "r17",
    "operations": [
      {
        "type": "put",
        "store": "steps",
        "records": [{
          "key": "step-3",
          "value": { "position": 3, "status": "complete", "summary": "Tests pass." }
        }]
      },
      {
        "type": "put",
        "store": "state",
        "records": [{
          "key": "workflow",
          "value": { "activeStep": "step-4", "phase": "execution" }
        }]
      }
    ]
  }
}
```

The matching `hostResponses["finish-step"]` entry is:

```json
{
  "result": {
    "revision": "r18",
    "results": [{ "written": 1 }, { "written": 1 }]
  }
}
```

If another writer changed the database after the read, `revisionConflict` leaves
both records unchanged. The server reads again, recomputes, and submits a new
request ID. It MUST NOT blindly overwrite newer data or reuse the old request ID
with changed arguments.

Database-wide conflict detection is deliberately simple. It protects decisions
based on missing records and range reads as well as existing records, at the cost
of conflicts from unrelated writes. Applications can use separate databases for
independent data, but lose atomicity between them. Per-record versions are a
possible later design if actual contention justifies them.

## Paged scans

A scan returns `{records, next}`. Each record contains `key` and, if requested,
`value`; index scans also include `indexKey`. Direction is `next` or `prev`.
`limit` is a positive upper bound on record count. Hosts may return fewer records
to fit response limits. `next` is null at the end or an opaque continuation token.
An empty page with a non-null token is forbidden. If the first eligible record
cannot fit, fail with `resultTooLarge` rather than skip it.

The caller passes the token as `after` in a subsequent scan with the same store,
index, range, direction, and `includeValues`; the page size may change. Tokens
bind the namespace, database incarnation, schema version, database revision,
query, and last ordering key. Continuation is exclusive of the last
`(indexKey, primaryKey)` pair, or primary key for store scans. Reverse scans use
the corresponding reverse ordering. The host validates every binding.

Paged scans are available in readonly transactions only. A continuation requires
the same database revision as its first page. Any intervening write produces
`scanInvalidated`; the caller restarts the scan and discards the partial result.
The host does not keep a transaction or database lock open between pages, and
MUST NOT silently continue over a different snapshot. This gives consistent
results when a scan completes, but a busy database can force repeated restarts.
Applications needing progress despite sustained writes may need a future
snapshot facility; this proposal does not promise one implicitly.

Tokens may expire or become unavailable after restart, reported as `cursorExpired`.
They are resource references, not durable application state. Multiple first-page
scans and point reads can share one readonly transaction and therefore one
snapshot. Subsequent pages from those scans either use that same revision or fail.

## Retry and durability

`requestId` is a caller-generated opaque unique identifier, distinct from a
JSON-RPC request ID. Creation, schema changes, readwrite transactions, and deletion
all require it. Deduplication is scoped to the storage namespace, across methods
and databases. The host compares the method and all semantic params, excluding
the request ID and transport metadata; JSON object member order is insignificant.
Same ID with different params returns `requestIdConflict`.

For an accepted mutation, the host durably records its terminal result or
execution error. A successful mutation and its receipt MUST commit atomically.
Concurrent identical retries execute once and return the same outcome. Replay
looks up the receipt before checking schema versions, revisions, or database
existence, so a successful retry still works after later changes or deletion.
Authorization is always checked first.

Malformed, unauthorized, and admission-limit failures may be rejected before
acceptance and need not create a receipt. Once accepted, conflicts and other
execution failures are terminal recorded outcomes for that ID. If receipt space
is unavailable, the host rejects before executing; it cannot commit and discard
the evidence needed to recover.

Receipts survive disconnect and restart and are retained for the namespace's
lifetime, including receipts for deleted databases. The host MUST NOT silently
expire them. This costs storage; quota accounting includes receipts. Explicit
namespace deletion removes both databases and receipts and revokes its binding,
so a stale request cannot recreate deleted state through the old application session binding.
A bounded receipt-retirement protocol is an open question, not a hidden TTL.

Success means the mutation and receipt are durably committed under the host's
documented storage guarantees, not merely queued in memory. A transport failure,
timeout, or cancellation leaves the outcome unknown to the caller. Retrying the
exact mutation with its original request ID resolves it. Cancellation cannot
undo a transaction already committed. The host must settle an accepted request
with one recorded outcome before an identical retry can execute.

These guarantees cover database mutations only. Updating a plan and editing a
workspace file are not one transaction. Controllers must still use the core
protocol's pending-transition and acknowledgment recovery. They can put their
application state and pending-transition record in the same storage transaction.

## Deletion, errors, and host policy

`frances/db/delete` accepts `databaseId`, `requestId`, and `expectedRevision`.
It atomically deletes the database and returns `{deleted: true}`. Revision is
required to avoid deleting state changed since the caller last observed it.
Old database IDs never refer to a recreation. Explicitly deleting a host session
also removes its session storage; it does not delete workspace storage.

Operation failures use JSON-RPC-shaped errors in `hostResponses`, with a machine-readable
`data.kind`. Proposed kinds are `notFound`, `permissionDenied`,
`unsupportedScope`, `schemaMismatch`, `revisionConflict`, `constraintViolation`,
`requestIdConflict`, `scanInvalidated`, `cursorExpired`, `quotaExceeded`,
`limitExceeded`, `resultTooLarge`, and `storageUnavailable`. Invalid wire shapes
use JSON-RPC invalid-params errors. Operation failures include `operationIndex`
and may identify the offending store, index, or input record. No failed
transaction returns a successful prefix. Diagnostics must not leak another
namespace's existence or data.

Storage host operations are infrastructure operations, not model-facing tools, and MUST NOT
recursively trigger tool hooks. Negotiation does not bypass host policy. A server
may expose application tools that use this storage; their user-visible effects
still require appropriate authorization descriptions. Reading a database does
not automatically insert its contents into model context.

The host SHOULD expose usage and explicit deletion controls by server and scope.
It MUST NOT silently evict durable workflow state to meet a quota. The physical
engine is an implementation choice; Frances can use turso without exposing its
SQL dialect or internal schema through the extension.

## Host-operation budget

| Work | Host operations after opening |
| --- | --- |
| Fetch known keys from several stores | One readonly transaction |
| Insert or replace a bounded batch across stores | One readwrite transaction |
| Read, run arbitrary application logic, conditionally write | Two transactions; additional attempts only on conflict |
| Read an indexed range | One request per bounded page |
| Retry a mutation with a lost response | One replay of the same request |
| Update several records and persist a context-transition proposal | One readwrite transaction in the same database |

Each host-operation round requires a server response followed by a client retry
of the originating RPC. The counts above describe storage operations, not extra
standalone network RPCs. Independent operations can share a continuation round;
read-dependent writes need a later round.

Operations are batched inside one method call, independently of JSON-RPC batch
support. SDKs SHOULD make this boundary explicit: a local builder accumulates
operations and one `execute` sends them. A callback that appears to allow arbitrary
`await` inside a transaction would hide round trips or imply locks this protocol
does not provide. Results are returned only for explicit reads; writes return
counts instead of echoing values. Keys can be allocated before building the batch.

## Decisions to review

1. Are session and optional workspace scopes sufficient, or is a separate
   server-wide scope needed for shared preferences and caches?
2. Is database-wide optimistic concurrency sufficient for expected workloads?
   It keeps the contract small but can cause avoidable retries.
3. Do long scans require pinned snapshots despite the resource-lifetime cost?
4. Should schema changes and data writes share one atomic request? Current
   structural changes are atomic, but arbitrary application transformations are
   separate operations.
5. Is lifetime receipt retention acceptable, or do we need explicit retirement
   with durable rejection of retired request IDs?
6. Specify complete schemas, numeric errors, stable server/workspace identity
   assignment, and continuation schemas before claiming interoperability. Session
   identity and resumption are defined by the core `frances/session` extension.

## Review scenarios

An implementation should demonstrate:

1. Two server identities using the same database name cannot see each other's data.
2. Context replacement and reconnection preserve session data; an unrelated host
   session cannot access it. Workspace data can be shared by authorized sessions.
3. A multi-store write is atomic, including unique-index maintenance and receipts.
4. A lost mutation response followed by restart and retry returns the original
   outcome, even if the current revision or schema has changed.
5. Reusing a request ID with different arguments never executes the new request.
6. Two writers using the same expected revision cannot both commit.
7. Missing point reads are distinct from stored null; bulk results preserve order.
8. Index pages with duplicate index keys neither skip nor repeat records in either
   direction. An intervening write explicitly invalidates continuation.
9. A failed schema update leaves the old schema and all records intact.
10. Request, response, execution, record, and receipt limits never produce partial
    commits or success without durable recovery evidence.
11. Retrying deletion succeeds through its receipt without affecting a recreated
    database with the same name.
12. A hook can persist state while the host awaits its response without deadlock.
13. A storage operation inherits the originating request's application session;
    interleaved requests on one stdio process cannot change its namespace.
14. A lost continuation round reuses the durable storage request ID and returns
    the recorded outcome without committing the transaction twice.
