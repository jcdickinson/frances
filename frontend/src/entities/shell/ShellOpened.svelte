<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { commands as backend } from '../../bindings';
  import type { EntityState } from '../../stores/entities.svelte';
  import { streamItems, subscribeStream, unsubscribeStream } from '../../stores/entityStreams.svelte';
  import { unwrap } from '../../types';
  import { asShellSnapshot, asShellStreamItem, shellStateView } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asShellSnapshot(entity.snapshot));
  const stateView = $derived(shellStateView(snapshot.state, entity.lifecycle));

  onMount(() => void subscribeStream(entity.id, true));
  onDestroy(() => void unsubscribeStream(entity.id));

  // Collapse the item stream into renderable blocks: consecutive text
  // concatenates, a dropped marker becomes an elision line.
  type Block = { key: number; kind: 'text' | 'gap'; value: string };
  const blocks = $derived.by(() => {
    const out: Block[] = [];
    for (const item of streamItems(entity.id)) {
      const payload = asShellStreamItem(item.payload);
      if ('dropped' in payload) {
        out.push({ key: item.seq, kind: 'gap', value: formatBytes(payload.dropped) });
      } else if (out.length > 0 && out[out.length - 1].kind === 'text') {
        out[out.length - 1].value += payload.text;
      } else {
        out.push({ key: item.seq, kind: 'text', value: payload.text });
      }
    }
    return out;
  });

  // The exact tool result the model received, fetched on demand.
  let modelView = $state<string | null>(null);
  let showModelView = $state(false);

  async function toggleModelView(): Promise<void> {
    showModelView = !showModelView;
    if (showModelView && modelView === null) {
      const digest = unwrap(await backend.readEntityArtifact(entity.id, 'llm_digest'));
      modelView = typeof digest === 'string' ? digest : '(not available)';
    }
  }

  function formatBytes(count: number): string {
    if (count >= 1024 * 1024) return `${(count / (1024 * 1024)).toFixed(1)} MiB`;
    if (count >= 1024) return `${(count / 1024).toFixed(1)} KiB`;
    return `${count} B`;
  }
</script>

<div class="entity-pane content">
  <div class="entity-header">
    <span class="pill {stateView.tone}">[{stateView.label}]</span>
    <span class="command">{snapshot.cmd}</span>
  </div>
  <div class="entity-meta">
    {formatBytes(snapshot.bytesTotal)} total
    {#if snapshot.bytesDropped > 0}· {formatBytes(snapshot.bytesDropped)} elided{/if}
    · <button class="tail" onclick={() => void toggleModelView()}>
      {showModelView ? 'output' : 'model view'}
    </button>
  </div>
  {#if showModelView}
    <pre>{modelView ?? 'loading…'}</pre>
  {:else}
    {#each blocks as block (block.key)}
      {#if block.kind === 'gap'}
        <div class="collapsed">… {block.value} elided …</div>
      {:else}
        <pre>{block.value}</pre>
      {/if}
    {/each}
  {/if}
</div>
