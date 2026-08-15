import type { JsonValue } from '../../bindings';

// Hand-written: the chat producer is JS (frances:v1/messages), so this
// shape has no Rust source for specta to export. Keep in sync with
// crates/frances-workflow/assets/frances/v1/messages.js.
export type ChatSnapshot = {
  source: 'user' | 'assistant' | 'internal';
  text: string;
};

export function asChatSnapshot(value: JsonValue): ChatSnapshot {
  return value as ChatSnapshot;
}
