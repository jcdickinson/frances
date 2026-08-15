<script lang="ts">
  import type { Snippet } from 'svelte';
  import Spinner from './Spinner.svelte';
  import type { EntityState } from '../stores/entities.svelte';

  /** The gutter mark: a tooltip, a spinner while the entity is still
   *  running, and the kind's icon. The spinner's slot is always in the
   *  layout so icons don't shift when one settles. Sections with no
   *  entity (an error, a diff) pass no entity and never spin. */
  let { title, entity, children }: {
    title: string;
    entity?: EntityState;
    children: Snippet;
  } = $props();

  const pending = $derived(entity !== undefined && entity.lifecycle !== 'settled');
</script>

<span class="sigil-mark" {title}>
  <span class="sigil-spin">
    {#if pending}<Spinner size={14} thick={5} />{/if}
  </span>
  {@render children()}
</span>
