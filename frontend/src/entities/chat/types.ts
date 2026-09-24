import type { JsonValue } from '../../bindings';

// Keep this snapshot shape in sync with crates/frances-session/src/runtime/driver.rs.
export type ChatSnapshot = {
  source: 'user' | 'assistant' | 'reasoning' | 'internal';
  text: string;
};

export function asChatSnapshot(value: JsonValue): ChatSnapshot {
  return value as ChatSnapshot;
}
