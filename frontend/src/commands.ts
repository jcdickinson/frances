export interface Command {
  id: string;
  title: string;
  run: () => void | Promise<void>;
}

export function filterCommands(commands: Command[], query: string): Command[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return commands;
  return commands.filter(
    (command) =>
      command.title.toLowerCase().includes(needle) || command.id.toLowerCase().includes(needle),
  );
}
