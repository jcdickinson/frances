<script lang="ts">
  import type { DiffOp } from '../bindings';
  import { viewsFor } from '../entities/registry';
  import { entity } from '../stores/entities.svelte';
  import type { Section } from '../types';

  let { section }: { section: Section } = $props();
  let expanded = $state(false);

  const TAIL_LINES = 10;

  const referenced = $derived(
    section.kind.type === 'entity_ref' ? entity(section.kind.entity_id) : undefined,
  );
  const Sigil = $derived(referenced ? viewsFor(referenced.kind).Sigil : undefined);

  function tailedText(): { hidden: number; text: string } {
    const lines = section.text.replace(/\n$/, '').split('\n');
    if (expanded || lines.length <= TAIL_LINES) return { hidden: 0, text: lines.join('\n') };
    return {
      hidden: lines.length - TAIL_LINES,
      text: lines.slice(-TAIL_LINES).join('\n'),
    };
  }

  function diffLine(line: DiffOp): { className: string; prefix: string; text: string } {
    if ('Context' in line) {
      return {
        className: 'context',
        prefix: String(line.Context.line).padStart(4),
        text: line.Context.text,
      };
    }
    if ('Added' in line) return { className: 'added', prefix: '+    ', text: line.Added };
    return { className: 'removed', prefix: '-    ', text: line.Removed };
  }
</script>

<article class:streaming={!section.closed} class:truncated={section.truncated} class="section">
  <div class="sigil" aria-hidden="true">
    {#if Sigil && referenced}<Sigil entity={referenced} />{/if}
  </div>
  <div class="content">
    {#if section.kind.type === 'error'}
      <div class="error">frances: error: {section.text}</div>
    {:else if section.kind.type === 'tool_use'}
      <div class="tool">
        → {section.kind.name}{section.kind.detail ? ` ${section.kind.detail}` : ''}
      </div>
    {:else if section.kind.type === 'json'}
      <div class="label">[{section.kind.tag}]</div>
      <pre>{JSON.stringify(section.kind.value, null, 2)}</pre>
    {:else if section.kind.type === 'reasoning'}
      <button class="tail" onclick={() => (expanded = !expanded)} title="Toggle full reasoning">
        <span class="pill {section.kind.state === 'Streaming' ? 'pending' : 'settled'}">
          [{section.kind.state === 'Streaming' ? 'reasoning' : 'reasoned'}]
        </span>
      </button>
      {@const tail = tailedText()}
      {#if tail.hidden > 0}<div class="collapsed">… [{tail.hidden} earlier lines]</div>{/if}
      {#if tail.text}<pre class="reasoning">{tail.text}</pre>{/if}
    {:else if section.kind.type === 'entity_ref'}
      {#if referenced}
        {@const Inline = viewsFor(referenced.kind).Inline}
        <Inline entity={referenced} />
      {:else}
        <!-- Attach ordering makes this unreachable in practice; keep a
             visible row rather than a blank if it ever regresses. -->
        <div class="label">[entity {section.kind.entity_id}]</div>
      {/if}
    {:else if section.kind.type === 'diff'}
      <div class="diff">
        {#each section.kind.lines as line}
          {@const display = diffLine(line)}
          <div class={display.className}><span>{display.prefix}</span> {display.text}</div>
        {/each}
      </div>
    {/if}
    {#if section.truncated}<span class="truncation">[truncated]</span>{/if}
  </div>
</article>
