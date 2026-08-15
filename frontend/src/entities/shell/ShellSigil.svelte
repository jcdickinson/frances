<script lang="ts">
  import Terminal from '@lucide/svelte/icons/terminal';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asShellSnapshot, shellStateView } from './types';

  let { entity }: { entity: EntityState } = $props();

  // Same tone the inline row's pill uses, so a failed command reads as
  // failed from the gutter alone.
  const tone = $derived(
    shellStateView(asShellSnapshot(entity.snapshot).state, entity.lifecycle).tone,
  );
</script>

<Terminal class="icon-{tone}" />
