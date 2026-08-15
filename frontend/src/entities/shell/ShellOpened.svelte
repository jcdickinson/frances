<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import Hr from '../../components/Hr.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { streamItems, subscribeStream, unsubscribeStream } from '../../stores/entityStreams.svelte';
  import { asShellSnapshot, asShellStreamItem, shellStateView } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asShellSnapshot(entity.snapshot));
  const stateView = $derived(shellStateView(snapshot.state, entity.lifecycle));
  const running = $derived(snapshot.state.type === 'running' && entity.lifecycle === 'live');

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

  function formatBytes(count: number): string {
    if (count >= 1024 * 1024) return `${(count / (1024 * 1024)).toFixed(1)} MiB`;
    if (count >= 1024) return `${(count / 1024).toFixed(1)} KiB`;
    return `${count} B`;
  }
</script>

<div class="entity-pane shell-opened">
  <div class="entity-header">{snapshot.cmd}</div>
  <Hr />
  <div class="output">
    {#each blocks as block (block.key)}
      {#if block.kind === 'gap'}
        <div class="collapsed">… {block.value} elided …</div>
      {:else}
        <pre>{block.value}</pre>
      {/if}
    {/each}
  </div>
  {#if !running}
    <Hr />
    <div class="entity-meta">
      <span class="pill {stateView.tone}">[{stateView.label}]</span>
      · {formatBytes(snapshot.bytesTotal)} total
      {#if snapshot.bytesDropped > 0}· {formatBytes(snapshot.bytesDropped)} elided{/if}
    </div>
  {/if}
</div>
