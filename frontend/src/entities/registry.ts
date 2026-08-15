import type { Component } from 'svelte';
import type { EntityState } from '../stores/entities.svelte';
import { asChatSnapshot } from './chat/types';
import ChatInline from './chat/ChatInline.svelte';
import ChatSigil from './chat/ChatSigil.svelte';
import FallbackInline from './FallbackInline.svelte';
import FallbackOpened from './FallbackOpened.svelte';
import FileInline from './file/FileInline.svelte';
import FileOpened from './file/FileOpened.svelte';
import FileSigil from './file/FileSigil.svelte';
import ShellInline from './shell/ShellInline.svelte';
import ShellOpened from './shell/ShellOpened.svelte';
import ShellSigil from './shell/ShellSigil.svelte';

export type EntityViews = {
  /** Transcript gutter mark. Absent means an empty gutter. */
  Sigil?: Component<{ entity: EntityState }>;
  /** Transcript rendering of an EntityRef. Snapshot-only when settled. */
  Inline: Component<{ entity: EntityState }>;
  /** Tab rendering. Owns the catch-up stream subscription. Absent means
   *  the kind is not openable as a tab (nothing should call openTab). */
  Opened?: Component<{ entity: EntityState }>;
  /** True when the entity has nothing worth a transcript row yet — an
   *  assistant message before its first token, say. The row (gutter
   *  included) is skipped entirely until this goes false. Absent means
   *  the kind always renders. */
  isEmpty?: (entity: EntityState) => boolean;
};

const kinds: Record<string, EntityViews> = {
  shell: { Sigil: ShellSigil, Inline: ShellInline, Opened: ShellOpened },
  file: { Sigil: FileSigil, Inline: FileInline, Opened: FileOpened },
  chat: {
    Sigil: ChatSigil,
    Inline: ChatInline,
    isEmpty: (entity) => asChatSnapshot(entity.snapshot).text.trim() === '',
  },
};

/** Unknown kinds degrade to a JSON dump instead of a blank row. */
export function viewsFor(kind: string): EntityViews {
  return kinds[kind] ?? { Inline: FallbackInline, Opened: FallbackOpened };
}

/** Whether the entity's own renderer considers it not worth showing. */
export function isEmptyEntity(entity: EntityState): boolean {
  return viewsFor(entity.kind).isEmpty?.(entity) ?? false;
}
