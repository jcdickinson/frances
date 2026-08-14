<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import type { EntityState } from '../stores/entities.svelte';
  import { streamItems, subscribeStream, unsubscribeStream } from '../stores/entityStreams.svelte';

  let { entity }: { entity: EntityState } = $props();

  onMount(() => void subscribeStream(entity.id, true));
  onDestroy(() => void unsubscribeStream(entity.id));
</script>

<div class="entity-pane content">
  <div class="entity-header">
    <span class="pill {entity.lifecycle === 'live' ? 'pending' : 'settled'}">[{entity.kind}]</span>
  </div>
  <pre>{JSON.stringify(entity.snapshot, null, 2)}</pre>
  {#each streamItems(entity.id) as item (item.seq)}
    <pre>{JSON.stringify(item.payload)}</pre>
  {/each}
</div>
