<script lang="ts">
  import { Progress } from 'bits-ui';

  let { value = 0, max = 100, label }: {
    value?: number;
    max?: number;
    label?: string;
  } = $props();

  const percent = $derived(max > 0 ? (100 * value) / max : 0);
</script>

<Progress.Root class="trough" {value} {max} aria-label={label}>
  <div class="fill" style:width="max(0px, calc({percent}% - 4px))"></div>
</Progress.Root>
