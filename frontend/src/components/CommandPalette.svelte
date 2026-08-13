<script lang="ts">
  import { Command, Dialog } from 'bits-ui';
  import type { Command as PaletteCommand } from '../commands';

  let { commands, onrun, onclose }: {
    commands: PaletteCommand[];
    onrun: (command: PaletteCommand) => void;
    onclose: () => void;
  } = $props();
</script>

<Dialog.Root open onOpenChange={(open) => open || onclose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="palette-overlay" />
    <Dialog.Content class="palette" aria-label="Command palette">
      <Command.Root loop>
        <Command.Input placeholder="type a command…" aria-label="Command search" />
        <Command.List>
          <Command.Viewport>
            {#each commands as command (command.id)}
              <Command.Item
                value={command.id}
                keywords={[command.title]}
                onSelect={() => onrun(command)}
              >
                <span class="title">{command.title}</span>
                <span class="id">{command.id}</span>
              </Command.Item>
            {/each}
            <Command.Empty>no matching commands</Command.Empty>
          </Command.Viewport>
        </Command.List>
      </Command.Root>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
