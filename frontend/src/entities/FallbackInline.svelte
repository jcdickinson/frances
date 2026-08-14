<script lang="ts">
  import type { EntityState } from '../stores/entities.svelte';
  import { openTab } from '../stores/tabs.svelte';

  let { entity }: { entity: EntityState } = $props();
</script>

<button class="tail" onclick={() => openTab(entity.id)} title="Open entity">
  <span class="pill {entity.lifecycle === 'live' ? 'pending' : 'settled'}">[{entity.kind}]</span>
  <span class="command">{entity.id}</span>
</button>
