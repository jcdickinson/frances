<script lang="ts">
  import { commands } from '../bindings';
  import { session } from '../stores/entities.svelte';
  import { unwrap } from '../types';

  const status = $derived(session()?.mcp);
  const selection = $derived(JSON.stringify(status?.selection ?? { presets: [], servers: [] }));
  let presets = $state<string[]>([]);
  let servers = $state<string[]>([]);
  let saving = $state(false);
  let error = $state('');

  $effect(() => {
    const selected = JSON.parse(selection);
    presets = selected.presets;
    servers = selected.servers;
  });

  async function apply(): Promise<void> {
    saving = true;
    error = '';
    try {
      unwrap(await commands.selectMcp({ presets, servers }));
    } catch (cause) {
      error = String(cause);
    } finally {
      saving = false;
    }
  }
</script>

<details class="mcp-panel">
  <summary>MCP · {status?.active_servers.length ?? 0} active</summary>
  {#if status}
    <p>{status.active_servers.join(' + ') || 'No servers enabled'}</p>
    <fieldset disabled={saving}>
      <legend>Presets</legend>
      {#each Object.entries(status.presets) as [name, members] (name)}
        <label title={members?.join(', ')}>
          <input type="checkbox" value={name} bind:group={presets} /> {name}
        </label>
      {:else}
        <p>No presets configured.</p>
      {/each}
    </fieldset>
    <fieldset disabled={saving}>
      <legend>Additional servers</legend>
      {#each status.available_servers as name (name)}
        <label><input type="checkbox" value={name} bind:group={servers} /> {name}</label>
      {:else}
        <p>Add servers in your Frances configuration.</p>
      {/each}
    </fieldset>
    <p>Apply starts a fresh model context and carries over the conversation.</p>
    <button disabled={saving} onclick={apply}>{saving ? 'Connecting…' : 'Apply / refresh'}</button>
    {#if error}<p role="alert">{error}</p>{/if}
  {/if}
</details>

<style>
  .mcp-panel {
    padding: 0.75rem;
    font-size: 0.85rem;
  }
  summary {
    cursor: pointer;
  }
  fieldset {
    border: 0;
    padding: 0;
    margin: 0.75rem 0;
  }
  label {
    display: block;
    margin: 0.35rem 0;
  }
  p {
    overflow-wrap: anywhere;
  }
</style>
