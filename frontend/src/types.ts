export type Source = 'user' | 'assistant' | 'internal';

export type ShellState = 'Running' | 'Success' | { Exit: number };
export type ReasoningState = 'Streaming' | 'Done';

export type DiffLine =
  | { Context: { text: string; line: number } }
  | { Added: string }
  | { Removed: string };

export type SectionKind =
  | { type: 'markdown'; source: Source }
  | { type: 'error' }
  | { type: 'tool_use'; name: string; detail: string | null }
  | { type: 'json'; tag: string; value: unknown }
  | { type: 'shell_output'; state: ShellState; cmd: string }
  | { type: 'reasoning'; state: ReasoningState }
  | { type: 'diff'; lines: DiffLine[] };

export interface Section {
  id: number;
  kind: SectionKind;
  text: string;
  closed: boolean;
  truncated: boolean;
}

export interface Usage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  cached_input_tokens: number;
}

export type SurfaceCommand =
  | { type: 'set_footer'; text: string }
  | { type: 'clear_footer' }
  | { type: 'set_title'; title: string | null };

export type UiEvent =
  | { type: 'reset' }
  | { type: 'replay_end' }
  | { type: 'section_append'; id: number; kind: SectionKind; delta: string }
  | { type: 'section_close'; id: number; truncated: boolean }
  | { type: 'usage'; usage: Usage }
  | { type: 'surface'; command: SurfaceCommand }
  | { type: 'error'; message: string }
  | { type: 'permission'; prompt: string };

export interface AppInfo {
  sessionId: string;
  title: string | null;
}
