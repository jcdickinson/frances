<script lang="ts">
  import { workspace } from '../stores/entities.svelte';

  const directories = $derived(workspace()?.directories ?? []);

  function basename(path: string): string {
    return path.replace(/\/+$/, '').split('/').pop() || path;
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
