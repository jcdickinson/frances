<script lang="ts">
  import { Slider } from 'bits-ui';

  let { value = $bindable(0), min = 0, max = 100, step = 1, disabled = false, label, onchange }: {
    value?: number;
    min?: number;
    max?: number;
    step?: number;
    disabled?: boolean;
    label?: string;
    onchange?: (value: number) => void;
  } = $props();
</script>

<Slider.Root
  type="single"
  class="rail"
  bind:value
  {min}
  {max}
  {step}
  {disabled}
  aria-label={label}
  onValueChange={onchange}
>
  <Slider.Thumb index={0} class="thumb" />
</Slider.Root>
