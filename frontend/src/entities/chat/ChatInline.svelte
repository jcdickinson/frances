<script lang="ts">
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  // Snapshot-only: the producer refreshes `text` on every delta, so live
  // and settled render the same way. No stream subscription needed.
  const snapshot = $derived(asChatSnapshot(entity.snapshot));
</script>

<div
  class:streaming={entity.lifecycle === 'live'}
  class:user={snapshot.source === 'user'}
  class="prose"
>
  {snapshot.text}
</div>
