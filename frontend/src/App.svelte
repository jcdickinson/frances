<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { commands as backend, events, type SectionKind, type UiEvent } from './bindings';
  import CommandPalette from './components/CommandPalette.svelte';
  import SectionView from './components/SectionView.svelte';
  import Sidebar from './components/Sidebar.svelte';
  import Spinner from './components/Spinner.svelte';
  import FallbackOpened from './entities/FallbackOpened.svelte';
  import { viewsFor } from './entities/registry';
  import { entity, session, upsertEntity } from './stores/entities.svelte';
  import { applyStreamItem } from './stores/entityStreams.svelte';
  import { activeTab, openTabs } from './stores/tabs.svelte';
  import type { Command } from './commands';
  import { type Section, unwrap } from './types';

  let sections = $state<Section[]>([]);
  let sessionId = $state('starting…');
  let input = $state('');
  let permission = $state<string | null>(null);
  let paletteOpen = $state(false);
  let nextSectionId = 0;
  let scrollback = $state<HTMLElement | undefined>();
  let textarea: HTMLTextAreaElement;

  // Grow the textarea with its content; CSS max-height caps it at 5 lines.
  $effect(() => {
    void input;
    textarea.style.height = 'auto';
    textarea.style.height = `${textarea.scrollHeight}px`;
  });

  onMount(() => {
    let unlisten: (() => void) | undefined;

    void (async () => {
      unlisten = await events.uiEvent.listen(({ payload }) => applyEvent(payload));
      const info = unwrap(await backend.frontendReady());
      sessionId = info.sessionId;
      await tick();
      textarea.focus();
    })().catch(showError);

    const keydown = (event: KeyboardEvent) => {
      if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'i') {
        event.preventDefault();
        void backend.toggleDevtools().catch(showError);
      } else if (event.ctrlKey && event.key.toLowerCase() === 'p') {
        event.preventDefault();
        if (paletteOpen) {
          void closePalette();
        } else {
          paletteOpen = true;
        }
      } else if (paletteOpen) {
        // The palette dialog owns the keyboard while open (Escape closes it).
      } else if (event.key === 'Escape') {
        event.preventDefault();
        void backend.interrupt().catch(showError);
      } else if (permission && event.altKey && event.key.toLowerCase() === 'y') {
        event.preventDefault();
        void answerPermission('yes');
      } else if (permission && event.altKey && event.key.toLowerCase() === 'n') {
        event.preventDefault();
        void answerPermission('no');
      }
    };
    window.addEventListener('keydown', keydown);

    return () => {
      unlisten?.();
      window.removeEventListener('keydown', keydown);
    };
  });

  async function applyEvent(event: UiEvent): Promise<void> {
    if (event.type === 'reset') {
      sections = [];
    } else if (event.type === 'section') {
      pushSection(event.kind);
    } else if (event.type === 'entity_upsert') {
      upsertEntity({
        id: event.entity_id,
        kind: event.kind,
        lifecycle: event.lifecycle,
        snapshot: event.snapshot,
      });
    } else if (event.type === 'entity_stream') {
      applyStreamItem(event.entity_id, event.seq, event.payload);
    } else if (event.type === 'error') {
      addError(event.message);
    } else if (event.type === 'permission') {
      permission = event.prompt;
    }

    await tick();
    scrollback?.scrollTo({ top: scrollback.scrollHeight });
  }

  function pushSection(kind: SectionKind): void {
    sections = [...sections, { id: nextSectionId++, kind }];
  }

  function addError(message: string): void {
    pushSection({ type: 'error', text: message });
  }

  function showError(error: unknown): void {
    addError(error instanceof Error ? error.message : String(error));
  }

  function enterSubmits(event: KeyboardEvent): void {
    if (event.key !== 'Enter' || event.shiftKey || event.altKey) return;
    event.preventDefault();
    textarea.form?.requestSubmit();
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const text = input.trim();
    if (!text) return;
    input = '';

    if (permission) {
      await answerPermission('chat', text);
    } else {
      await backend.sendPrompt(text).catch(showError);
    }
  }

  async function answerPermission(decision: 'yes' | 'no' | 'chat', text = input): Promise<void> {
    const details = text.trim() || null;
    await backend.respondPermission(decision, details).then(unwrap).catch(showError);
    permission = null;
    input = '';
    await tick();
    textarea.focus();
  }

  const commands: Command[] = [
    {
      id: 'workspace::save',
      title: 'Save Workspace',
      run: async () => void unwrap(await backend.saveWorkspace()),
    },
  ];

  function runCommand(command: Command): void {
    void closePalette().then(() => command.run()).catch(showError);
  }

  async function closePalette(): Promise<void> {
    paletteOpen = false;
    await tick();
    textarea.focus();
  }

  const busy = $derived(session()?.busy ?? null);

  function tokenStatus(): string {
    const usage = session()?.usage;
    if (!usage) return 'tokens: —';
    return `tokens: ${usage.total_tokens} total · ${usage.prompt_tokens} prompt (${usage.cached_input_tokens} cached) · ${usage.completion_tokens} completion`;
  }
</script>

<Sidebar />

<main>
  <!-- Every tab stays in the DOM (scroll position, subscriptions, and
       transcript state survive switches); the inactive ones are just
       display:none, which also keeps them out of main's grid. -->
  <section
    class="scrollback"
    class:hidden-tab={activeTab() !== null}
    bind:this={scrollback}
    aria-live="polite"
  >
    <header>
      <div>frances session {sessionId}</div>
      <div>
        Enter to send. Shift+Enter or Alt+Enter for newline. Esc to interrupt. Ctrl-P for
        commands.
      </div>
    </header>

    {#each sections as section (section.id)}
      <SectionView {section} />
    {/each}
  </section>
  {#each openTabs() as id (id)}
    {@const opened = entity(id)}
    <section class="scrollback" class:hidden-tab={activeTab() !== id}>
      {#if opened}
        {@const Opened = viewsFor(opened.kind).Opened ?? FallbackOpened}
        <Opened entity={opened} />
      {:else}
        <div class="label">[entity {id}]</div>
      {/if}
    </section>
  {/each}

  {#if paletteOpen}
    <CommandPalette {commands} onrun={runCommand} onclose={() => void closePalette()} />
  {/if}

  {#if permission}
    <div class="permission">
      <div><span>permission:</span> {permission}</div>
      <div class="permission-actions">
        <button onclick={() => answerPermission('yes')}>Allow <kbd>Alt Y</kbd></button>
        <button onclick={() => answerPermission('no')}>Deny <kbd>Alt N</kbd></button>
        <span>Enter sends typed text back to chat</span>
      </div>
    </div>
  {/if}

  <footer>
    <form class="input-shell" onsubmit={submit}>
      <textarea
        bind:this={textarea}
        bind:value={input}
        onkeydown={enterSubmits}
        placeholder={permission ? 'Add details, or type a message for chat…' : 'type a message…'}
        rows="1"
        aria-label="Message"
      ></textarea>
      {#if busy}<div class="busy"><Spinner size={16} thick={5} /> [{busy}]</div>{/if}
      <button type="submit" class="send">Send</button>
    </form>
    <div class="tokens">{tokenStatus()}</div>
  </footer>
</main>
