<script lang="ts">
  import type { DiffOp } from '../bindings';
  import { isEmptyEntity, viewsFor } from '../entities/registry';
  import { entity } from '../stores/entities.svelte';
  import type { Section } from '../types';

  let { section }: { section: Section } = $props();

  const referenced = $derived(
    section.kind.type === 'entity_ref' ? entity(section.kind.entity_id) : undefined,
  );
  const Sigil = $derived(referenced ? viewsFor(referenced.kind).Sigil : undefined);
  // An entity with nothing to show yet (an assistant message before its
  // first token) takes no row at all — gutter included — until it fills.
  const empty = $derived(referenced !== undefined && isEmptyEntity(referenced));

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

{#if !empty}
  <article class="section">
    <div class="sigil" aria-hidden="true">
      {#if Sigil && referenced}<Sigil entity={referenced} />{/if}
    </div>
    <div class="content">
      {#if section.kind.type === 'error'}
        <div class="error">frances: error: {section.kind.text}</div>
      {:else if section.kind.type === 'json'}
        <div class="label">[{section.kind.tag}]</div>
        <pre>{JSON.stringify(section.kind.value, null, 2)}</pre>
      {:else if section.kind.type === 'entity_ref'}
        {#if referenced}
          {@const Inline = viewsFor(referenced.kind).Inline}
          <Inline entity={referenced} />
        {:else}
          <!-- Attach ordering makes this unreachable in practice; keep
               a visible row rather than a blank if it ever regresses. -->
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
    </div>
  </article>
{/if}
