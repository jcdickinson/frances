<script lang="ts">
  import type { HTMLButtonAttributes } from 'svelte/elements';

  let { variant, pill = false, icon = false, pressed = false, children, ...rest }:
    & HTMLButtonAttributes
    & {
      variant?: 'primary' | 'danger' | 'success' | 'warning';
      pill?: boolean;
      icon?: boolean;
      pressed?: boolean;
    } = $props();
</script>

<button class={['raised', variant, { pill, icon, pressed }]} {...rest}>
  {@render children?.()}
</button>
