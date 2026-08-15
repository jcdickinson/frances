import type { Component } from 'svelte';

/** Values collected from a command's form, keyed by the `name` attributes
 *  the form's inputs declare. Each command turns them into the typed
 *  options of its own execute function. */
export type CommandValues = Record<string, string>;

interface CommandBase {
  id: string;
  title: string;
}

/** Enter runs it; there is nothing to fill in. */
export interface PlainCommand extends CommandBase {
  form?: undefined;
  run: () => void | Promise<void>;
}

/** Tab opens `form` in a popup and submitting runs the command with its
 *  values. The form owns its fields only — the host supplies the title,
 *  the close button, the `<form>` element and the submit button.
 *
 *  `runBare` is Enter's way past the form. Commands that can't do anything
 *  useful without values leave it out, and Enter opens the form too. */
export interface FormCommand extends CommandBase {
  form: Component;
  run: (values: CommandValues) => void | Promise<void>;
  runBare?: () => void | Promise<void>;
}

/** Pairing the form with the shape of `run` keeps the two in step: a
 *  command with no form can never be handed values. */
export type Command = PlainCommand | FormCommand;
