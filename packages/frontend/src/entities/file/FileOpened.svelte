<script lang="ts">
  import CodeView from '../../components/CodeView.svelte';
  import Hr from '../../components/Hr.svelte';
  import type { EntityState } from '../../stores/entities.svelte';
  import { asFileSnapshot, lineCount } from './types';

  let { entity }: { entity: EntityState } = $props();

  const snapshot = $derived(asFileSnapshot(entity.snapshot));
</script>

<div class="entity-pane file-opened">
  <div class="entity-header">{snapshot.path}</div>
  <Hr />
  <div class="output">
    <CodeView rows={snapshot.rows} />
  </div>
  <Hr />
  <div class="entity-meta">{lineCount(snapshot.rows)} lines read</div>
</div>
