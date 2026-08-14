<script lang="ts">
  let { options, value = $bindable(), onchange }: {
    options: { value: string; label: string }[];
    value?: string;
    onchange?: (value: string) => void;
  } = $props();

  function select(next: string): void {
    value = next;
    onchange?.(next);
  }
</script>

<div class="seg" role="group">
  {#each options as option (option.value)}
    <button
      class={['raised', { pressed: option.value === value }]}
      aria-pressed={option.value === value}
      onclick={() => select(option.value)}
    >
      {option.label}
    </button>
  {/each}
</div>
