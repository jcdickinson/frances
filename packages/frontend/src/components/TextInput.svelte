<script lang="ts">
  import type { HTMLInputAttributes } from 'svelte/elements';

  let { value = $bindable(''), ...rest }: HTMLInputAttributes & {
    type?: 'text' | 'email' | 'password' | 'search';
  } = $props();
</script>

<input type="text" class="recessed" bind:value {...rest} />
