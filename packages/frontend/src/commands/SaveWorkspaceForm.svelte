<script lang="ts">
  import PathInput from '../components/PathInput.svelte';
</script>

<label class="command-field">
  <span>filename:</span>
  <PathInput name="path" value="workspace.toml" required />
</label>
