<script lang="ts">
  import Markdown from '../../components/Markdown.svelte';
  import Spinner from '../../components/Spinner.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asChatSnapshot } from './types';

  let { entity }: { entity: EntityState } = $props();

  // Snapshot-only: the producer refreshes `text` on every delta, so live
  // and settled render the same way. No stream subscription needed.
  const snapshot = $derived(asChatSnapshot(entity.snapshot));
  const live = $derived(entity.lifecycle === 'live');

  // Reasoning is bulky scratch work: show the tail until the user asks
  // for the rest.
  const TAIL_LINES = 10;
  let expanded = $state(false);

  const tail = $derived.by(() => {
    const lines = snapshot.text.replace(/\n$/, '').split('\n');
    if (expanded || lines.length <= TAIL_LINES) return { hidden: 0, text: lines.join('\n') };
    return { hidden: lines.length - TAIL_LINES, text: lines.slice(-TAIL_LINES).join('\n') };
  });
</script>

{#if snapshot.source === 'user'}
  <div class="prose user">{snapshot.text}</div>
{:else if snapshot.source === 'reasoning'}
  <button class="tail" onclick={() => (expanded = !expanded)} title="Toggle full reasoning">
    <span class="pill {live ? 'pending' : 'settled'}">[{live ? 'reasoning' : 'reasoned'}]</span>
  </button>
  {#if tail.hidden > 0}<div class="collapsed">… [{tail.hidden} earlier lines]</div>{/if}
  {#if tail.text}<pre class="reasoning">{tail.text}</pre>{/if}
{:else}
  <div class="prose">
    <Markdown text={snapshot.text} done={!live} />
    {#if live}<Spinner size={14} thick={5} />{/if}
  </div>
{/if}
