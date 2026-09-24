import type { Result, SectionKind } from './bindings.ts';

/** A rendered section: the wire kind plus a local key. Sections are
 *  one-shot, so `id` exists only to key the `{#each}`. */
export interface Section {
  id: number;
  kind: SectionKind;
}

/** Throw a command's error so callers can handle every failure in one catch. */
export function unwrap<T>(result: Result<T, string>): T {
  if (result.status === 'error') throw new Error(result.error);
  return result.data;
}
