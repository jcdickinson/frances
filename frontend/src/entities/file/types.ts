import type { JsonValue } from '../../bindings';
import type { CodeRow } from '../../components/CodeView.svelte';

// Hand-written: the file producer is JS (workflow-side file.js), so this
// shape has no Rust source for specta to export. Keep in sync with
// `_pushReadEntity` in
// crates/frances-workflow/assets/frances/v1/tools/file.js.
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
