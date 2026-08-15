<script lang="ts">
  import FileText from '@lucide/svelte/icons/file-text';
  import type { EntityState } from '../../stores/entities.svelte';

  // Every read looks the same in the gutter; the entity is unused.
  let _props: { entity: EntityState } = $props();
</script>

<FileText class="icon-accent" />
