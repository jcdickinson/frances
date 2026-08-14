// Frontend mirror of the runtime's published entities. One-way: the
// event listener writes via upsertEntity, components read projections.
import { SvelteMap } from 'svelte/reactivity';
import type { EntityWire } from '../bindings';

const entities = new SvelteMap<EntityWire['type'], EntityWire>();

export function upsertEntity(entity: EntityWire): void {
  entities.set(entity.type, entity);
}

export function workspace(): Extract<EntityWire, { type: 'workspace' }> | undefined {
  const entity = entities.get('workspace');
  return entity?.type === 'workspace' ? entity : undefined;
}

export function session(): Extract<EntityWire, { type: 'session' }> | undefined {
  const entity = entities.get('session');
  return entity?.type === 'session' ? entity : undefined;
}
