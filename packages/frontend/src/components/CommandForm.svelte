<script lang="ts">
  import { Dialog } from 'bits-ui';
  import X from '@lucide/svelte/icons/x';
  import Button from './Button.svelte';
  import Hr from './Hr.svelte';
  import type { CommandValues, FormCommand } from '../commands';

  /** The chrome around a command's form: the title, the close button, the
   *  <form> itself and its submit button. The command contributes fields
   *  and nothing else; their `name` attributes become the values. */
  let { command, onsubmit, onclose }: {
    command: FormCommand;
    onsubmit: (values: CommandValues) => void;
    onclose: () => void;
  } = $props();

  const Fields = $derived(command.form);

  let form = $state<HTMLFormElement | null>(null);

  // The dialog would otherwise open on the close button; the point of the
  // popup is the first field.
  function focusFirstField(event: Event): void {
    const field = form?.querySelector('input, textarea, select');
    if (!(field instanceof HTMLElement)) return;
    event.preventDefault();
    field.focus();
  }

  function submit(event: SubmitEvent & { currentTarget: HTMLFormElement }): void {
    event.preventDefault();
    const entries = [...new FormData(event.currentTarget)];
    onsubmit(Object.fromEntries(entries.map(([name, value]) => [name, String(value)])));
  }
</script>

<Dialog.Root open onOpenChange={(open) => open || onclose()}>
  <Dialog.Portal>
    <Dialog.Overlay class="palette-overlay" />
    <Dialog.Content class="palette command-popup" onOpenAutoFocus={focusFirstField}>
      <div class="command-form-head">
        <Dialog.Title>{command.title}</Dialog.Title>
        <Dialog.Close>
          {#snippet child({ props })}
            <Button {...props} icon aria-label="Close"><X size={14} /></Button>
          {/snippet}
        </Dialog.Close>
      </div>
      <Hr />
      <form class="command-form" bind:this={form} onsubmit={submit}>
        <Fields />
        <div class="command-form-actions">
          <Button type="submit" variant="primary">Save</Button>
        </div>
      </form>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
