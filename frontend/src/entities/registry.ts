import type { Component } from 'svelte';
import type { EntityState } from '../stores/entities.svelte';
import ChatInline from './chat/ChatInline.svelte';
import ChatSigil from './chat/ChatSigil.svelte';
import FallbackInline from './FallbackInline.svelte';
import FallbackOpened from './FallbackOpened.svelte';
import ShellInline from './shell/ShellInline.svelte';
import ShellOpened from './shell/ShellOpened.svelte';

export type EntityViews = {
  /** Transcript gutter mark. Absent means an empty gutter. */
  Sigil?: Component<{ entity: EntityState }>;
  /** Transcript rendering of an EntityRef. Snapshot-only when settled. */
  Inline: Component<{ entity: EntityState }>;
  /** Tab rendering. Owns the catch-up stream subscription. Absent means
   *  the kind is not openable as a tab (nothing should call openTab). */
  Opened?: Component<{ entity: EntityState }>;
};

const kinds: Record<string, EntityViews> = {
  shell: { Inline: ShellInline, Opened: ShellOpened },
  chat: { Sigil: ChatSigil, Inline: ChatInline },
};

/** Unknown kinds degrade to a JSON dump instead of a blank row. */
export function viewsFor(kind: string): EntityViews {
  return kinds[kind] ?? { Inline: FallbackInline, Opened: FallbackOpened };
}
