<script lang="ts">
  import type { EntityState } from '../stores/entities.svelte';
  import { openTab } from '../stores/tabs.svelte';

  let { entity }: { entity: EntityState } = $props();
</script>

<button class="tail" onclick={() => openTab(entity.id)} title="Open entity">
  <span class="path">{entity.kind}</span>
  <span class="collapsed">{entity.id}</span>
</button>
