<script lang="ts">
  import { type Command, filterCommands } from './commands';

  let { commands, onrun, onclose }: {
    commands: Command[];
    onrun: (command: Command) => void;
    onclose: () => void;
  } = $props();

  let query = $state('');
  let selected = $state(0);
  let input: HTMLInputElement;

  const filtered = $derived(filterCommands(commands, query));

  $effect(() => {
    query;
    selected = 0;
  });

  $effect(() => {
    input.focus();
  });

  function keydown(event: KeyboardEvent): void {
    event.stopPropagation();
    if (event.key === 'Escape' || (event.ctrlKey && event.key.toLowerCase() === 'p')) {
      event.preventDefault();
      onclose();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      if (filtered.length) selected = (selected + 1) % filtered.length;
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      if (filtered.length) selected = (selected + filtered.length - 1) % filtered.length;
    } else if (event.key === 'Enter') {
      event.preventDefault();
      const command = filtered[selected];
      if (command) onrun(command);
    }
  }
</script>

<div class="palette-overlay" onclick={onclose} role="presentation">
  <div
    class="palette"
    onclick={(event) => event.stopPropagation()}
    onkeydown={keydown}
    role="dialog"
    aria-label="Command palette"
    tabindex="-1"
  >
    <input
      bind:this={input}
      bind:value={query}
      placeholder="type a command…"
      aria-label="Command search"
    />
    <ul>
      {#each filtered as command, index (command.id)}
        <li>
          <button
            class:selected={index === selected}
            onclick={() => onrun(command)}
            onpointerenter={() => (selected = index)}
          >
            <span class="title">{command.title}</span>
            <span class="id">{command.id}</span>
          </button>
        </li>
      {:else}
        <li class="empty">no matching commands</li>
      {/each}
    </ul>
  </div>
</div>
