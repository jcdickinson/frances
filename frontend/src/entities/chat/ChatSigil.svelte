<script lang="ts">
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  const sigils = { user: '>', assistant: '◆', internal: '' } as const;
  const sigil = $derived(sigils[asChatSnapshot(entity.snapshot).source] ?? '');
</script>

{sigil}
