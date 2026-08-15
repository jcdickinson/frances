import ResumeSessionForm from './ResumeSessionForm.svelte';
import SaveWorkspaceForm from './SaveWorkspaceForm.svelte';
import { saveWorkspace } from './workspace';
import type { Command } from './types';

export type { Command, CommandValues, FormCommand, PlainCommand } from './types';

export const commands: Command[] = [
  {
    id: 'workspace::save',
    title: 'Save Workspace',
    form: SaveWorkspaceForm,
    run: async (values) => {
      await saveWorkspace({ path: values.path });
    },
    // Without a filename the backend asks for one with a save dialog.
    runBare: async () => {
      await saveWorkspace();
    },
  },
  {
    id: 'session::resume',
    title: 'Resume Session',
    form: ResumeSessionForm,
    run: () => {},
  },
];
