export interface Command {
  id: string;
  title: string;
  run: () => void | Promise<void>;
}
