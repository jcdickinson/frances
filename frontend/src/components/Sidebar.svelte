<script lang="ts">
  import { entity, workspace } from '../stores/entities.svelte';
  import { activeTab, closeTab, focusTab, openTabs } from '../stores/tabs.svelte';
  import { asShellSnapshot } from '../entities/shell/types';

  const directories = $derived(workspace()?.directories ?? []);

  function basename(path: string): string {
    return path.replace(/\/+$/, '').split('/').pop() || path;
  }

  /** Trigger label: something recognisable per kind, id as last resort. */
  function tabTitle(id: string): string {
    const state = entity(id);
    if (!state) return id;
    if (state.kind === 'shell') return asShellSnapshot(state.snapshot).cmd;
    return state.kind;
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
  <h2>Tabs</h2>
  <ul class="entity-tab-list">
    <li class="entity-tab-row">
      <button
        class="entity-tab"
        class:active={activeTab() === null}
        onclick={() => focusTab(null)}
      >transcript</button>
    </li>
    {#each openTabs() as id (id)}
      <li class="entity-tab-row">
        <button
          class="entity-tab"
          class:active={activeTab() === id}
          onclick={() => focusTab(id)}
          title={tabTitle(id)}
        >{tabTitle(id)}</button>
        <button
          class="entity-tab-close"
          onclick={() => closeTab(id)}
          aria-label="Close tab"
          title="Close tab"
        >×</button>
      </li>
    {/each}
  </ul>

  <h2>Directories</h2>
  <ul>
    {#each directories as directory (directory)}
      <li title={directory}>{basename(directory)}</li>
    {/each}
  </ul>
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
