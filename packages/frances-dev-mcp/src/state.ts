import { dirname } from 'node:path';

export type State = { counter: number; fixture: Record<string, unknown> };

export class StateStore {
  #state: State;
  #path: string | undefined;
  #queue = Promise.resolve();

  private constructor(state: State, path?: string) {
    this.#state = state;
    this.#path = path;
  }

  static async open(path?: string): Promise<StateStore> {
    let state: State = { counter: 0, fixture: {} };
    if (path) {
      try {
        state = JSON.parse(await Deno.readTextFile(path));
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
        console.error(`Creating state file: ${path}`);
        await Deno.mkdir(dirname(path), { recursive: true });
        const store = new StateStore(state, path);
        await store.#save(state);
        return store;
      }
      if (
        !state || !Number.isSafeInteger(state.counter) || state.counter < 0 ||
        !state.fixture || typeof state.fixture !== 'object' || Array.isArray(state.fixture)
      ) {
        throw new Error(
          'State file must contain { "counter": nonnegative integer, "fixture": object }',
        );
      }
    }
    return new StateStore(state, path);
  }

  get snapshot(): State {
    return structuredClone(this.#state);
  }

  async increment(): Promise<State> {
    const previous = this.#queue;
    let release!: () => void;
    this.#queue = new Promise<void>((resolve) => release = resolve);
    await previous;
    try {
      if (this.#state.counter === Number.MAX_SAFE_INTEGER) throw new Error('Counter exhausted');
      const next = { ...this.#state, counter: this.#state.counter + 1 };
      await this.#save(next);
      this.#state = next;
      return this.snapshot;
    } finally {
      release();
    }
  }

  async #save(state: State) {
    if (!this.#path) return;
    const temp = await Deno.makeTempFile({ dir: dirname(this.#path), prefix: '.dev-mcp-' });
    try {
      const file = await Deno.open(temp, { write: true, truncate: true });
      try {
        const bytes = new TextEncoder().encode(JSON.stringify(state, null, 2) + '\n');
        let written = 0;
        while (written < bytes.length) written += await file.write(bytes.subarray(written));
        await file.sync();
      } finally {
        file.close();
      }
      await Deno.rename(temp, this.#path);
    } catch (error) {
      try {
        await Deno.remove(temp);
      } catch (cleanupError) {
        console.error('Cannot remove temporary state file:', cleanupError);
      }
      throw error;
    }
  }
}
