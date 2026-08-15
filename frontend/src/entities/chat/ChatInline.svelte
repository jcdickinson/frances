<script lang="ts">
  import Markdown from '../../components/Markdown.svelte';
  import Spinner from '../../components/Spinner.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  // Snapshot-only: the producer refreshes `text` on every delta, so live
  // and settled render the same way. No stream subscription needed.
  const snapshot = $derived(asChatSnapshot(entity.snapshot));
</script>

{#if snapshot.source === 'user'}
  <div class="prose user">{snapshot.text}</div>
{:else}
  <div class="prose">
    <Markdown text={snapshot.text} done={entity.lifecycle !== 'live'} />
    {#if entity.lifecycle === 'live'}<Spinner size={14} thick={5} />{/if}
  </div>
{/if}
