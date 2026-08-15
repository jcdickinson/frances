<script lang="ts">
  import Brain from '@lucide/svelte/icons/brain';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Info from '@lucide/svelte/icons/info';
  import Sparkles from '@lucide/svelte/icons/sparkles';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  const source = $derived(asChatSnapshot(entity.snapshot).source);
</script>

{#if source === 'user'}
  <ChevronRight class="icon-accent" />
{:else if source === 'assistant'}
  <Sparkles class="icon-success" />
{:else if source === 'reasoning'}
  <Brain class="icon-muted" />
{:else}
  <Info class="icon-muted" />
{/if}
