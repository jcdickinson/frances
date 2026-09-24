<script lang="ts">
  import { Switch } from 'bits-ui';

  let { checked = $bindable(false), disabled = false, label, onchange }: {
    checked?: boolean;
    disabled?: boolean;
    label?: string;
    onchange?: (checked: boolean) => void;
  } = $props();
</script>

<Switch.Root class="toggle" bind:checked {disabled} aria-label={label} onCheckedChange={onchange}>
  <Switch.Thumb class="thumb" />
</Switch.Root>
