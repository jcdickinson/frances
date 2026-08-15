<script lang="ts">
  import Terminal from '@lucide/svelte/icons/terminal';
  import SigilMark from '../../components/SigilMark.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asShellSnapshot, shellStateView } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asShellSnapshot(entity.snapshot));
  // Same tone the inline row's pill uses, so a failed command reads as
  // failed from the gutter alone.
  const stateView = $derived(shellStateView(snapshot.state, entity.lifecycle));
</script>

<SigilMark {entity} title="{snapshot.cmd} — {stateView.label}">
  <Terminal class="icon-{stateView.tone}" />
</SigilMark>
