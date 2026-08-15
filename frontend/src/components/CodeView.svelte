<script module lang="ts">
  /** One row of a code view: a file line, or the elision between two
   *  non-adjacent ranges. `anchor` is the line's Frances anchor word —
   *  present for in-repo reads, where it is what the model edits by. */
  export type CodeRow =
    | { kind: 'line'; line: number | null; anchor: string | null; text: string }
    | { kind: 'gap' };
</script>

<script lang="ts">
  // Plain text today. Syntax highlighting slots in here: swap the text
  // span for a run of per-token spans and leave everything else alone.
  let { rows }: { rows: CodeRow[] } = $props();
</script>

<div class="code">
  {#each rows as row, i (i)}
    {#if row.kind === 'gap'}
      <div class="code-gap">⋯</div>
    {:else}
      <span class="code-gutter" title={row.anchor ?? undefined}>{row.line ?? ''}</span>
      <span class="code-text">{row.text}</span>
    {/if}
  {/each}
</div>
