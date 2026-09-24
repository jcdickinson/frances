<script lang="ts">
  import type { HTMLTextareaAttributes } from 'svelte/elements';

  let { value = $bindable(''), ...rest }: HTMLTextareaAttributes = $props();
</script>

<textarea class="recessed" bind:value {...rest}></textarea>
