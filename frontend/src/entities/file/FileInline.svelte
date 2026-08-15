<script lang="ts">
  import CodeView from '../../components/CodeView.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { openTab } from '../../stores/tabs.svelte';
  import { asFileSnapshot, lineCount } from './types';

  let { entity }: { entity: EntityState } = $props();

  // A read is settled the moment it is published — no live state here.
  const snapshot = $derived(asFileSnapshot(entity.snapshot));

  // A whole-file read is a wall of code in the transcript; show the head
  // and let the tab carry the rest.
  const HEAD_ROWS = 12;
  const head = $derived(snapshot.rows.slice(0, HEAD_ROWS));
  const hidden = $derived(lineCount(snapshot.rows) - lineCount(head));
</script>

<button class="tail" onclick={() => openTab(entity.id)} title="Open file">
  <span class="pill settled">[read]</span>
  <span class="command">{snapshot.path}</span>
</button>
<CodeView rows={head} />
{#if hidden > 0}<div class="collapsed">… {hidden} more lines</div>{/if}
