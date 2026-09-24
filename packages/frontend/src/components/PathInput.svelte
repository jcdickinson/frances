<script lang="ts">
  import type { HTMLInputAttributes } from 'svelte/elements';
  import TextInput from './TextInput.svelte';

  /** A filesystem path field. Plain text entry today; path completion will
   *  hang off this component. */
  let { value = $bindable(''), ...rest }: Omit<HTMLInputAttributes, 'type'> = $props();
</script>

<TextInput bind:value {...rest} />
