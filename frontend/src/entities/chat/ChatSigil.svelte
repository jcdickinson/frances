<script lang="ts">
  import Brain from '@lucide/svelte/icons/brain';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Info from '@lucide/svelte/icons/info';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import SigilMark from '../../components/SigilMark.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  const source = $derived(asChatSnapshot(entity.snapshot).source);
</script>

{#if source === 'user'}
  <SigilMark {entity} title="You"><ChevronRight class="icon-accent" /></SigilMark>
{:else if source === 'assistant'}
  <SigilMark {entity} title="Assistant"><Sparkles class="icon-success" /></SigilMark>
{:else if source === 'reasoning'}
  <SigilMark {entity} title="Reasoning"><Brain class="icon-muted" /></SigilMark>
{:else}
  <SigilMark {entity} title={source}><Info class="icon-muted" /></SigilMark>
{/if}
