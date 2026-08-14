import { SvelteMap } from 'svelte/reactivity';
import { commands as backend, type JsonValue } from '../bindings';

export type StreamItem = { seq: number; payload: JsonValue };

type StreamState = {
  items: StreamItem[];
  /** Highest seq seen — drops duplicates across the catch-up/live splice. */
  lastSeq: number;
  refs: number;
};

const streams = new SvelteMap<string, StreamState>();

/** Route point for `entity_stream` events. Ignored unless subscribed. */
export function applyStreamItem(entityId: string, seq: number, payload: JsonValue): void {
  const stream = streams.get(entityId);
  if (!stream || seq <= stream.lastSeq) return;
  streams.set(entityId, {
    ...stream,
    lastSeq: seq,
    items: [...stream.items, { seq, payload }],
  });
}

export function streamItems(entityId: string): StreamItem[] {
  return streams.get(entityId)?.items ?? [];
}

/**
 * Refcounted subscription. The first subscriber decides the mode:
 * `catchUp` replays everything persisted (a tab opening); without it
 * the stream tails from now (an inline view watching from birth).
 */
export async function subscribeStream(entityId: string, catchUp: boolean): Promise<void> {
  const existing = streams.get(entityId);
  if (existing) {
    streams.set(entityId, { ...existing, refs: existing.refs + 1 });
    return;
  }
  streams.set(entityId, { items: [], lastSeq: 0, refs: 1 });
  await backend.subscribeEntity(entityId, catchUp);
}

export async function unsubscribeStream(entityId: string): Promise<void> {
  const existing = streams.get(entityId);
  if (!existing) return;
  if (existing.refs > 1) {
    streams.set(entityId, { ...existing, refs: existing.refs - 1 });
    return;
  }
  streams.delete(entityId);
  await backend.unsubscribeEntity(entityId);
}
