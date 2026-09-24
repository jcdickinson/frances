<script lang="ts">
  let { size = 28, thick = 7 }: {
    size?: number;
    thick?: number;
  } = $props();
</script>

<div
  class="spinner"
  style:--spinner-size="{size}px"
  style:--spinner-thick="{thick}px"
  aria-hidden="true"
></div>
