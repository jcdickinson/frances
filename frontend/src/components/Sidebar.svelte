<script lang="ts">
  import { Tabs } from 'bits-ui';
  import { viewsFor } from '../entities/registry';
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
  <h2>Directories</h2>
  <ul>
    {#each directories as directory (directory)}
      <li title={directory}>{basename(directory)}</li>
    {/each}
  </ul>

  {#if openTabs().length > 0}
    <h2>Open</h2>
    <Tabs.Root
      value={activeTab() ?? undefined}
      onValueChange={(value) => focusTab(value)}
      orientation="vertical"
      class="entity-tabs"
    >
      <Tabs.List class="entity-tab-list">
        {#each openTabs() as id (id)}
          <div class="entity-tab-row">
            <Tabs.Trigger value={id} class="entity-tab" title={tabTitle(id)}>
              {tabTitle(id)}
            </Tabs.Trigger>
            <button
              class="entity-tab-close"
              onclick={() => closeTab(id)}
              aria-label="Close tab"
              title="Close tab"
            >×</button>
          </div>
        {/each}
      </Tabs.List>
      {#each openTabs() as id (id)}
        {@const state = entity(id)}
        <Tabs.Content value={id} class="entity-tab-content">
          {#if state}
            {@const Opened = viewsFor(state.kind).Opened}
            <Opened entity={state} />
          {:else}
            <div class="label">[entity {id}]</div>
          {/if}
        </Tabs.Content>
      {/each}
    </Tabs.Root>
  {/if}
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
