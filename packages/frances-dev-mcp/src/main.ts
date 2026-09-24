import { StateStore } from './state.ts';
import { failure, http, object, respond } from './server.ts';

async function stdio(store: StateStore) {
  const active = new Map<string | number, AbortController>();
  const tasks = new Set<Promise<void>>();
  const writer = Deno.stdout.writable.getWriter();
  const encoder = new TextEncoder();
  const send = (message: unknown) => writer.write(encoder.encode(JSON.stringify(message) + '\n'));
  let buffer = '';
  const dispatch = async (line: string) => {
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch (error) {
      console.error('Invalid JSON:', error);
      await send(failure(null, -32700, 'Parse error'));
      return;
    }
    if (object(message) && message.method === 'notifications/cancelled' && object(message.params)) {
      const id = message.params.requestId;
      if (typeof id === 'string' || typeof id === 'number') active.get(id)?.abort();
      return;
    }
    const id = object(message) ? message.id : undefined;
    const controller = new AbortController();
    const hasId = typeof id === 'string' || typeof id === 'number';
    if (hasId && active.has(id)) {
      await send(failure(id, -32600, 'Request ID already active'));
      return;
    }
    if (hasId) active.set(id, controller);
    try {
      const reply = await respond(message, store, controller.signal);
      if (reply) await send(reply);
    } finally {
      if (hasId) active.delete(id);
    }
  };
  for await (const chunk of Deno.stdin.readable.pipeThrough(new TextDecoderStream())) {
    buffer += chunk;
    let end: number;
    while ((end = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, end);
      buffer = buffer.slice(end + 1);
      if (!line.trim()) continue;
      const task = dispatch(line).catch((error) => {
        console.error('stdio request failed:', error);
        Deno.exitCode = 1;
      });
      tasks.add(task);
      void task.then(() => tasks.delete(task));
    }
  }
  if (buffer.trim()) await dispatch(buffer);
  for (const controller of active.values()) controller.abort();
  await Promise.all(tasks);
  writer.releaseLock();
}

async function main() {
  let transport = 'stdio';
  let port = 3001;
  let statePath: string | undefined;
  for (let i = 0; i < Deno.args.length; i++) {
    const arg = Deno.args[i];
    switch (arg) {
      case '--stdio':
        transport = 'stdio';
        break;
      case '--http':
        transport = 'http';
        break;
      case '--port':
        port = Number(Deno.args[++i]);
        if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('Invalid port');
        break;
      case '--state':
        statePath = Deno.args[++i];
        if (!statePath) throw new Error('--state requires a file path');
        break;
      case '--help':
        console.log('frances-dev-mcp [--stdio | --http] [--port 3001] [--state PATH]');
        return;
      default:
        throw new Error(`Unknown argument: ${arg}`);
    }
  }
  const store = await StateStore.open(statePath);
  if (!statePath) console.error('Using ephemeral state; pass --state PATH to persist it');
  if (transport === 'stdio') {
    await stdio(store);
  } else {
    const server = Deno.serve({
      hostname: '127.0.0.1',
      port,
      onListen: ({ port }) => console.error(`frances-dev-mcp: http://127.0.0.1:${port}/mcp`),
    }, (request) => http(request, store));
    await server.finished;
  }
}

if (import.meta.main) {
  main().catch((error) => {
    console.error(error);
    Deno.exitCode = 1;
  });
}
