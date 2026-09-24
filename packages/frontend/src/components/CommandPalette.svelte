<script lang="ts">
  import { tick } from 'svelte';
  import { Command, Dialog } from 'bits-ui';
  import Hr from './Hr.svelte';
  import TextInput from './TextInput.svelte';
  import type { Command as PaletteCommand, FormCommand } from '../commands';

  let { commands, onrun, onform, onclose }: {
    commands: PaletteCommand[];
    onrun: (run: () => void | Promise<void>) => void;
    onform: (command: FormCommand) => void;
    onclose: () => void;
  } = $props();

  // The child snippet replaces bits-ui's own input, so the search value
  // has to be bound through to it by hand.
  let search = $state('');
  let selected = $state('');
  let list = $state<HTMLElement | null>(null);

  // Enter runs the command outright where that means something, and opens
  // the form where it doesn't.
  function activate(command: PaletteCommand): void {
    if (!command.form) {
      onrun(command.run);
    } else if (command.runBare) {
      onrun(command.runBare);
    } else {
      onform(command);
    }
  }

  // Tab hands the highlighted command's form to the host; commands without
  // one leave Tab alone.
  function keydown(event: KeyboardEvent): void {
    if (event.key !== 'Tab' || event.shiftKey) return;
    const command = commands.find((candidate) => candidate.id === selected);
    if (!command?.form) return;
    event.preventDefault();
    onform(command);
  }

  // bits-ui scrolls the selection into view by looking up [data-selected]
  // before the DOM has caught up, so it moves the item that *was* selected.
  // A neighbouring row is already on screen and it looks fine; the loop's
  // wrap from the last row to the first doesn't scroll at all. Do it here,
  // once the attribute is actually on the new row.
  async function scrollSelectionIntoView(): Promise<void> {
    await tick();
    list?.querySelector('[data-selected]')?.scrollIntoView({ block: 'nearest' });
  }
</script>

<Dialog.Root open onOpenChange={(open) => open || onclose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="palette-overlay" />
    <Dialog.Content class="palette" aria-label="Command palette" onkeydown={keydown}>
      <Command.Root loop bind:value={selected} onValueChange={() => void scrollSelectionIntoView()}>
        <div class="palette-search">
          <Command.Input bind:value={search} placeholder="type a command…" aria-label="Command search">
            {#snippet child({ props })}
              <TextInput {...props} bind:value={search} />
            {/snippet}
          </Command.Input>
        </div>
        <Hr />
        <Command.List bind:ref={list}>
          <Command.Viewport>
            {#each commands as command (command.id)}
              <Command.Item
                value={command.id}
                keywords={[command.title]}
                onSelect={() => activate(command)}
              >
                <span class="title">{command.title}</span>
                <span class="id">{command.id}</span>
              </Command.Item>
            {/each}
            <Command.Empty>no matching commands</Command.Empty>
          </Command.Viewport>
        </Command.List>
        <Hr />
        <div class="palette-hint">
          <kbd>enter</kbd> to activate, <kbd>tab</kbd> to type parameters
        </div>
      </Command.Root>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
