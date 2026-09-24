import { commands as backend } from '../bindings';
import { unwrap } from '../types';

export interface SaveWorkspaceOptions {
  /** Where to write the workspace file, relative to the primary directory. */
  path: string;
}

/** Without options the backend opens a save dialog. Returns the saved
 *  path, or null if that dialog was cancelled. */
export async function saveWorkspace(options?: SaveWorkspaceOptions): Promise<string | null> {
  return unwrap(await backend.saveWorkspace(options?.path ?? null));
}
