<script lang="ts">
  import FileText from '@lucide/svelte/icons/file-text';
  import SigilMark from '../../components/SigilMark.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asFileSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asFileSnapshot(entity.snapshot));
</script>

<SigilMark {entity} title="Read {snapshot.path}">
  <FileText class="icon-accent" />
</SigilMark>
