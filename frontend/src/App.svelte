<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, tick } from 'svelte';
  import CommandPalette from './CommandPalette.svelte';
  import SectionView from './SectionView.svelte';
  import type { Command } from './commands';
  import type { AppInfo, Section, UiEvent, Usage } from './types';

  let sections = $state<Section[]>([]);
  let sessionId = $state('starting…');
  let input = $state('');
  let status = $state<string | null>(null);
  let usage = $state<Usage | null>(null);
  let permission = $state<string | null>(null);
  let historyMode = $state(false);
  let paletteOpen = $state(false);
  let errorId = -1;
  let scrollback: HTMLElement;
  let textarea: HTMLTextAreaElement;

  onMount(() => {
    let unlisten: (() => void) | undefined;

    void (async () => {
      unlisten = await listen<UiEvent>('session-event', ({ payload }) => applyEvent(payload));
      const info = await invoke<AppInfo>('frontend_ready');
      sessionId = info.sessionId;
      await tick();
      textarea.focus();
    })().catch(showError);

    const keydown = (event: KeyboardEvent) => {
      if (event.ctrlKey && event.key.toLowerCase() === 'p') {
        event.preventDefault();
        paletteOpen = !paletteOpen;
      } else if (event.key === 'Escape') {
        event.preventDefault();
        if (historyMode) {
          historyMode = false;
        } else {
          void invoke('interrupt').catch(showError);
        }
      } else if (event.ctrlKey && event.key.toLowerCase() === 'o') {
        event.preventDefault();
        historyMode = !historyMode;
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
    } else if (event.type === 'section_append') {
      const existing = sections.find((section) => section.id === event.id);
      if (existing) {
        existing.kind = event.kind;
        existing.text += event.delta;
        sections = [...sections];
      } else {
        sections = [
          ...sections,
          { id: event.id, kind: event.kind, text: event.delta, closed: false, truncated: false },
        ];
      }
    } else if (event.type === 'section_close') {
      const section = sections.find((candidate) => candidate.id === event.id);
      if (section) {
        section.closed = true;
        section.truncated = event.truncated;
        sections = [...sections];
      }
    } else if (event.type === 'usage') {
      usage = event.usage;
    } else if (event.type === 'surface') {
      if (event.command.type === 'set_footer') status = event.command.text;
      if (event.command.type === 'clear_footer') status = null;
    } else if (event.type === 'error') {
      addError(event.message);
    } else if (event.type === 'permission') {
      permission = event.prompt;
    }

    await tick();
    if (!historyMode) scrollback?.scrollTo({ top: scrollback.scrollHeight });
  }

  function addError(message: string): void {
    sections = [
      ...sections,
      { id: errorId--, kind: { type: 'error' }, text: message, closed: true, truncated: false },
    ];
  }

  function showError(error: unknown): void {
    addError(error instanceof Error ? error.message : String(error));
  }

  async function submit(event: KeyboardEvent): Promise<void> {
    if (event.key !== 'Enter' || event.shiftKey || event.altKey) return;
    event.preventDefault();
    const text = input.trim();
    if (!text) return;
    input = '';

    if (permission) {
      await answerPermission('chat', text);
    } else {
      await invoke('send_prompt', { text }).catch(showError);
    }
  }

  async function answerPermission(decision: 'yes' | 'no' | 'chat', text = input): Promise<void> {
    const details = text.trim() || null;
    await invoke('respond_permission', { decision, details }).catch(showError);
    permission = null;
    input = '';
    await tick();
    textarea.focus();
  }

  const commands: Command[] = [
    {
      id: 'workspace::save',
      title: 'Save Workspace',
      run: () => invoke('save_workspace'),
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

  function tokenStatus(): string {
    if (!usage) return 'tokens: —';
    return `tokens: ${usage.total_tokens} total · ${usage.prompt_tokens} prompt (${usage.cached_input_tokens} cached) · ${usage.completion_tokens} completion`;
  }
</script>

<main>
  <section class="scrollback" bind:this={scrollback} aria-live="polite">
    <header>
      <div>frances session {sessionId}</div>
      <div>
        Enter to send. Shift+Enter or Alt+Enter for newline. Esc to interrupt. Ctrl-O for history.
        Ctrl-P for commands.
      </div>
    </header>

    {#each sections as section (section.id)}
      <SectionView {section} />
    {/each}
  </section>

  {#if paletteOpen}
    <CommandPalette {commands} onrun={runCommand} onclose={() => void closePalette()} />
  {/if}

  {#if historyMode}
    <div class="history-bar">history · Ctrl-O or Esc to return to live output</div>
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
    <div class="input-shell">
      <textarea
        bind:this={textarea}
        bind:value={input}
        onkeydown={submit}
        placeholder={permission ? 'Add details, or type a message for chat…' : 'type a message…'}
        rows="1"
        aria-label="Message"
      ></textarea>
      {#if status}<div class="busy"><span class="spinner">⠋</span> [{status}]</div>{/if}
    </div>
    <div class="tokens">{tokenStatus()}</div>
  </footer>
</main>
