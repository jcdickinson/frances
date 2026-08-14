import type { JsonValue } from '../../bindings';

// Hand-written: the shell producer is JS (workflow-side shell.js), so
// these shapes have no Rust source for specta to export. Keep in sync
// with `_openShellEntity` / `_settleShellEntity` in
// crates/frances-workflow/assets/frances/v1/tools/shell.js.
export type ShellState =
  | { type: 'running' }
  | { type: 'success' }
  | { type: 'exit'; code: number }
  | { type: 'killed' };

export type ShellSnapshot = {
  cmd: string;
  state: ShellState;
  bytesTotal: number;
  bytesDropped: number;
  /** Last few lines, small — the collapsed row's preview. */
  teaser: string;
};

export type ShellStreamItem = { text: string } | { dropped: number };

export function asShellSnapshot(value: JsonValue): ShellSnapshot {
  return value as ShellSnapshot;
}

export function asShellStreamItem(value: JsonValue): ShellStreamItem {
  return value as ShellStreamItem;
}

/**
 * Display label + tone for a shell's state. "Interrupted" is derived,
 * not stored: a settled entity whose state still reads `running` was
 * force-settled (crash, workflow teardown) mid-run.
 */
export function shellStateView(
  state: ShellState,
  lifecycle: 'live' | 'settled',
): { label: string; tone: string } {
  if (state.type === 'running') {
    return lifecycle === 'settled'
      ? { label: 'interrupted', tone: 'failure' }
      : { label: 'running', tone: 'pending' };
  }
  if (state.type === 'success') return { label: 'success', tone: 'success' };
  if (state.type === 'exit') return { label: `exit ${state.code}`, tone: 'failure' };
  return { label: 'killed', tone: 'failure' };
}
