import type { Result, SectionKind } from './bindings.ts';

/** A rendered section: the streamed wire kind plus accumulated text. */
export interface Section {
  id: number;
  kind: SectionKind;
  text: string;
  closed: boolean;
  truncated: boolean;
}

/** Throw a command's error so callers can handle every failure in one catch. */
export function unwrap<T>(result: Result<T, string>): T {
  if (result.status === 'error') throw new Error(result.error);
  return result.data;
}
