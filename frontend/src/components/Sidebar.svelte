<script lang="ts">
  import { viewsFor } from '../entities/registry';
  import { entity, workspace } from '../stores/entities.svelte';
  import { activeTab, closeTab, focusTab, openTabs } from '../stores/tabs.svelte';
  import { asShellSnapshot } from '../entities/shell/types';
  import { asFileSnapshot } from '../entities/file/types';

  const directories = $derived(workspace()?.directories ?? []);

  function basename(path: string): string {
    return path.replace(/\/+$/, '').split('/').pop() || path;
  }

  /** Trigger label: something recognisable per kind, id as last resort. */
  function tabTitle(id: string): string {
    const state = entity(id);
    if (!state) return id;
    if (state.kind === 'shell') return asShellSnapshot(state.snapshot).cmd;
    if (state.kind === 'file') return basename(asFileSnapshot(state.snapshot).path);
    return state.kind;
  }

  // Open tabs grouped into one category per entity kind. A category
  // only exists while it has tabs; new kinds get their raw kind string
  // as a label until they earn a nicer one.
  const KIND_LABELS: Record<string, string> = { shell: 'Shells', file: 'Files' };

  const groups = $derived.by(() => {
    const byKind = new Map<string, string[]>();
    for (const id of openTabs()) {
      const kind = entity(id)?.kind ?? 'entity';
      const ids = byKind.get(kind);
      if (ids) {
        ids.push(id);
      } else {
        byKind.set(kind, [id]);
      }
    }
    return [...byKind].map(([kind, ids]) => ({
      kind,
      label: KIND_LABELS[kind] ?? kind,
      ids,
    }));
  });

  // Collapsed-state per section, keyed by 'directories' or the kind.
  let collapsed = $state<Record<string, boolean>>({});

  function toggle(section: string): void {
    collapsed[section] = !collapsed[section];
  }

  // Temporary home for the theme toggle; auto = follow the OS preference.
  type Mode = 'auto' | 'dark' | 'light';
  const MODES: Mode[] = ['auto', 'dark', 'light'];
  let mode = $state<Mode>('auto');

  function setMode(next: Mode): void {
    mode = next;
    if (next === 'auto') {
      delete document.documentElement.dataset.mode;
    } else {
      document.documentElement.dataset.mode = next;
    }
  }

  let width = $state(240);
  let dragStart: { x: number; width: number } | null = null;

  function pointerdown(event: PointerEvent): void {
    dragStart = { x: event.clientX, width };
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }

  function pointermove(event: PointerEvent): void {
    if (!dragStart) return;
    width = Math.min(600, Math.max(140, dragStart.width + event.clientX - dragStart.x));
  }

  function pointerup(): void {
    dragStart = null;
  }
</script>

<aside class="sidebar" style:width="{width}px">
  <!-- The transcript sits alone at the top until directories become
       sessions with transcripts of their own. -->
  <ul class="entity-tab-list">
    <li class="entity-tab-row">
      <button
        class="entity-tab"
        class:active={activeTab() === null}
        onclick={() => focusTab(null)}
      >
        transcript
      </button>
    </li>
  </ul>

  <button class="side-section" onclick={() => toggle('directories')}>
    <span class="chevron">{collapsed['directories'] ? '▸' : '▾'}</span> Directories
  </button>
  {#if !collapsed['directories']}
    <ul class="side-list">
      {#each directories as directory (directory)}
        <li title={directory}>{basename(directory)}</li>
      {/each}
    </ul>
  {/if}

  {#each groups as group (group.kind)}
    <button class="side-section" onclick={() => toggle(group.kind)}>
      <span class="chevron">{collapsed[group.kind] ? '▸' : '▾'}</span> {group.label}
    </button>
    {#if !collapsed[group.kind]}
      <ul class="entity-tab-list">
        {#each group.ids as id (id)}
          {@const state = entity(id)}
          {@const Sigil = state && viewsFor(state.kind).Sigil}
          <li class="entity-tab-row">
            <button
              class="entity-tab"
              class:active={activeTab() === id}
              onclick={() => focusTab(id)}
              onauxclick={(event) => {
                if (event.button !== 1) return;
                event.preventDefault();
                closeTab(id);
              }}
              title={tabTitle(id)}
            >
              {#if Sigil && state}<span class="tab-sigil" aria-hidden="true"><Sigil
                    entity={state}
                  /></span>{/if}{tabTitle(id)}
            </button>
            <button
              class="entity-tab-close"
              onclick={() => closeTab(id)}
              aria-label="Close tab"
              title="Close tab"
            >
              ×
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  {/each}

  <div class="mode-toggle">
    {#each MODES as m (m)}
      <button class:active={mode === m} onclick={() => setMode(m)}>{m}</button>
    {/each}
  </div>
</aside>

<div
  class="sidebar-handle"
  role="separator"
  aria-orientation="vertical"
  aria-label="Resize sidebar"
  onpointerdown={pointerdown}
  onpointermove={pointermove}
  onpointerup={pointerup}
>
</div>
