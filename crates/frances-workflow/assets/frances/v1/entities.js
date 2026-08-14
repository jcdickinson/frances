// `frances:v1/entities` — entity producer verbs.
//
// `createEntity(kind, snapshot)` publishes a Live entity and returns a
// handle: `handle.id` (string), `handle.updateSnapshot(obj)`,
// `handle.append(obj)`, `handle.settle(finalSnapshot, { artifacts })`.
// Snapshots/payloads are opaque JSON — the frontend's per-kind
// components give them meaning. Reference the entity from the
// transcript with `new EntityRefSection({ id: handle.id })` from
// `frances:v1/sections`.

const { _createEntity } = globalThis.__frances_v1_stash__;

export function createEntity(kind, snapshot) {
  return _createEntity(kind, snapshot);
}
