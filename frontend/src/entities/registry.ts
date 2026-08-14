import type { Component } from 'svelte';
import type { EntityState } from '../stores/entities.svelte';
import FallbackInline from './FallbackInline.svelte';
import FallbackOpened from './FallbackOpened.svelte';
import ShellInline from './shell/ShellInline.svelte';
import ShellOpened from './shell/ShellOpened.svelte';

export type EntityViews = {
  /** Transcript rendering of an EntityRef. Snapshot-only when settled. */
  Inline: Component<{ entity: EntityState }>;
  /** Tab rendering. Owns the catch-up stream subscription. */
  Opened: Component<{ entity: EntityState }>;
};

const kinds: Record<string, EntityViews> = {
  shell: { Inline: ShellInline, Opened: ShellOpened },
};

/** Unknown kinds degrade to a JSON dump instead of a blank row. */
export function viewsFor(kind: string): EntityViews {
  return kinds[kind] ?? { Inline: FallbackInline, Opened: FallbackOpened };
}
