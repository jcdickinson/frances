import type { JsonValue } from '../../bindings';
import type { CodeRow } from '../../components/CodeView.svelte';

// Keep this snapshot shape in sync with crates/frances-harness/src/tools/mod.rs.
export type FileSnapshot = {
  /** Path exactly as the model asked for it. */
  path: string;
  rows: CodeRow[];
};

export function asFileSnapshot(value: JsonValue): FileSnapshot {
  return value as FileSnapshot;
}

export function lineCount(rows: CodeRow[]): number {
  return rows.filter((row) => row.kind === 'line').length;
}
