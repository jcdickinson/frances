import { SvelteMap } from 'svelte/reactivity';
import { commands as backend, type JsonValue } from '../bindings';

export type StreamItem = { seq: number; payload: JsonValue };

type Sub = {
  refs: number;
  /** Highest seq seen — drops duplicates across the catch-up/live splice. */
  lastSeq: number;
  /** Whether the backend subscription replayed history. */
  catchUp: boolean;
};

// Split on purpose: `items` is the reactive rendering surface, `subs`
// is plain bookkeeping. subscribe/unsubscribe are called from inside
// component `$effect`s — if they *read* reactive state they also
// *write*, the effect tracks the read, and the write re-triggers it
// forever (effect_update_depth_exceeded, which kills the whole
// reactive tree). Only ever read `subs` here; only ever write `items`.
const subs = new Map<string, Sub>();
const items = new SvelteMap<string, StreamItem[]>();

/** Route point for `entity_stream` events. Ignored unless subscribed. */
export function applyStreamItem(entityId: string, seq: number, payload: JsonValue): void {
  const sub = subs.get(entityId);
  if (!sub || seq <= sub.lastSeq) return;
  sub.lastSeq = seq;
  items.set(entityId, [...(items.get(entityId) ?? []), { seq, payload }]);
}

export function streamItems(entityId: string): StreamItem[] {
  return items.get(entityId) ?? [];
}

/**
 * Refcounted subscription. `catchUp` replays everything persisted (a
 * tab opening); without it the stream tails from now (an inline view
 * watching from birth). A catch-up subscriber arriving on top of a
 * tail-only subscription escalates it: accumulated items reset and the
 * backend replays from seq 1 so the tab shows full history.
 */
export async function subscribeStream(entityId: string, catchUp: boolean): Promise<void> {
  const existing = subs.get(entityId);
  if (existing) {
    existing.refs += 1;
    if (catchUp && !existing.catchUp) {
      existing.catchUp = true;
      existing.lastSeq = 0;
      items.set(entityId, []);
      await backend.subscribeEntity(entityId, true);
    }
    return;
  }
  subs.set(entityId, { refs: 1, lastSeq: 0, catchUp });
  items.set(entityId, []);
  await backend.subscribeEntity(entityId, catchUp);
}

export async function unsubscribeStream(entityId: string): Promise<void> {
  const sub = subs.get(entityId);
  if (!sub) return;
  sub.refs -= 1;
  if (sub.refs > 0) return;
  subs.delete(entityId);
  items.delete(entityId);
  await backend.unsubscribeEntity(entityId);
}
