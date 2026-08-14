<script lang="ts">
  import { untrack } from 'svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { streamItems, subscribeStream, unsubscribeStream } from '../../stores/entityStreams.svelte';
  import { openTab } from '../../stores/tabs.svelte';
  import { asShellSnapshot, asShellStreamItem, shellStateView } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asShellSnapshot(entity.snapshot));
  const stateView = $derived(shellStateView(snapshot.state, entity.lifecycle));

  // While live the inline view is an expanded streaming pane: tail-only
  // subscription (catchUp false — this view watched from the entity's
  // birth, catch-up reads are Opened's business). The effect re-runs on
  // settle and its cleanup drops the subscription. The store call is
  // untracked: this effect must depend on lifecycle/id ONLY — if it
  // picks up a dependency on the stream store, every arriving chunk
  // re-runs it, and the cleanup+resubscribe churn wipes the items.
  $effect(() => {
    if (entity.lifecycle !== 'live') return;
    const id = entity.id;
    untrack(() => void subscribeStream(id, false));
    return () => void unsubscribeStream(id);
  });

  const liveText = $derived(
    streamItems(entity.id)
      .map((item) => {
        const payload = asShellStreamItem(item.payload);
        return 'text' in payload ? payload.text : '';
      })
      .join(''),
  );
</script>

<button class="tail" onclick={() => openTab(entity.id)} title="Open shell output">
  <span class="pill {stateView.tone}">[{stateView.label}]</span>
  <span class="command">{snapshot.cmd}</span>
</button>
{#if entity.lifecycle === 'live'}
  {#if liveText}<pre>{liveText}</pre>{/if}
{:else if snapshot.teaser}
  <pre class="teaser">{snapshot.teaser}</pre>
{/if}
