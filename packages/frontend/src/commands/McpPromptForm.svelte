<script lang="ts">
  import { commands } from '../bindings';
  import { session } from '../stores/entities.svelte';
  import { unwrap } from '../types';

  type Prompt = {
    name: string;
    description?: string;
    arguments?: { name: string; description?: string; required?: boolean }[];
  };
  const servers = $derived(session()?.mcp.active_servers ?? []);
  let server = $state('');
  let name = $state('');
  let prompts = $state<Prompt[]>([]);
  let values = $state<Record<string, string>>({});
  let error = $state('');
  let loading = $state(false);
  const chosen = $derived(prompts.find((prompt) => prompt.name === name));

  $effect(() => {
    const selected = server;
    let current = true;
    prompts = [];
    name = '';
    values = {};
    error = '';
    loading = false;
    if (selected) {
      loading = true;
      commands.mcpPrompts(selected).then((result) => {
        if (current) prompts = unwrap(result) as unknown as Prompt[];
      }).catch((cause) => {
        if (current) error = String(cause);
      }).finally(() => {
        if (current) loading = false;
      });
    }
    return () => {
      current = false;
    };
  });
</script>

<label class="command-field">
  <span>Server</span>
  <select name="server" bind:value={server} required>
    <option value="">Choose an active MCP server</option>
    {#each servers as item (item)}<option value={item}>{item}</option>{/each}
  </select>
</label>
<label class="command-field">
  <span>Prompt</span>
  <select name="name" bind:value={name} required>
    <option value="">{loading ? 'Loading…' : 'Choose a prompt'}</option>
    {#each prompts as prompt (prompt.name)}<option value={prompt.name}>{prompt.name}</option>{/each}
  </select>
</label>
{#if chosen?.description}<p>{chosen.description}</p>{/if}
{#each chosen?.arguments ?? [] as argument (argument.name)}
  <label class="command-field">
    <span>{argument.name}</span>
    <input
      bind:value={values[argument.name]}
      required={argument.required ?? false}
      title={argument.description}
    />
  </label>
{/each}
<input
  type="hidden"
  name="arguments"
  value={JSON.stringify(
    Object.fromEntries(
      (chosen?.arguments ?? []).filter((arg) => values[arg.name] !== undefined).map((
        arg,
      ) => [arg.name, values[arg.name]]),
    ),
  )}
/>
{#if error}<p role="alert">{error}</p>{/if}
