import { SvelteMap } from 'svelte/reactivity';
import type { JsonValue, Lifecycle, SessionSnapshot, WorkspaceSnapshot } from '../bindings';

export type EntityState = {
  id: string;
  kind: string;
  lifecycle: Lifecycle;
  /** Opaque payload; per-kind components cast it (see entities/registry). */
  snapshot: JsonValue;
};

const entities = new SvelteMap<string, EntityState>();

export function upsertEntity(entity: EntityState): void {
  entities.set(entity.id, entity);
}

export function entity(id: string): EntityState | undefined {
  return entities.get(id);
}

function snapshotOfKind<T>(kind: string): T | undefined {
  for (const candidate of entities.values()) {
    if (candidate.kind === kind) return candidate.snapshot as T;
  }
  return undefined;
}

// Typed projections of the two Rust-produced singletons.
export function workspace(): WorkspaceSnapshot | undefined {
  return snapshotOfKind<WorkspaceSnapshot>('workspace');
}

export function session(): SessionSnapshot | undefined {
  return snapshotOfKind<SessionSnapshot>('session');
}
