import { StateStore } from './state.ts';

export const VERSION = '2026-07-28';
export const META = 'io.modelcontextprotocol/';
type ObjectValue = Record<string, unknown>;
type Id = string | number;
export type Reply =
  & { jsonrpc: '2.0'; id: Id | null }
  & (
    | { result: ObjectValue }
    | { error: { code: number; message: string; data?: unknown } }
  );

export function object(value: unknown): value is ObjectValue {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

export function failure(id: Id | null, code: number, message: string, data?: unknown): Reply {
  return { jsonrpc: '2.0', id, error: { code, message, ...(data === undefined ? {} : { data }) } };
}

const tools = [
  { name: 'echo', description: 'Echo text and return the startup fixture as structured data.' },
  { name: 'fail', description: 'Return a deliberate tool failure containing the supplied text.' },
  { name: 'wait', description: 'Wait before echoing text, for cancellation and timeout testing.' },
  { name: 'increment', description: 'Atomically increment the persisted test counter.' },
].map((tool) => ({
  ...tool,
  inputSchema: {
    type: 'object',
    properties: {
      ...(tool.name === 'increment' ? {} : { text: { type: 'string' } }),
      ...(tool.name === 'wait'
        ? { milliseconds: { type: 'integer', minimum: 0, maximum: 60000 } }
        : {}),
    },
    required: tool.name === 'increment'
      ? []
      : tool.name === 'wait'
      ? ['text', 'milliseconds']
      : ['text'],
    additionalProperties: false,
  },
}));

export async function respond(
  message: unknown,
  store: StateStore,
  signal: AbortSignal = new AbortController().signal,
): Promise<Reply | null> {
  if (!object(message) || message.jsonrpc !== '2.0' || typeof message.method !== 'string') {
    return failure(null, -32600, 'Invalid JSON-RPC request');
  }
  if (!('id' in message)) return null;
  if (typeof message.id !== 'string' && !Number.isSafeInteger(message.id)) {
    return failure(null, -32600, 'Invalid request ID');
  }
  const id = message.id as Id;
  const params = message.params ?? {};
  if (!object(params)) return failure(id, -32602, 'Expected object parameters');
  const meta = params._meta;
  const version = object(meta) ? meta[`${META}protocolVersion`] : undefined;
  if (version !== VERSION) {
    return failure(id, -32022, 'Unsupported protocol version', {
      supported: [VERSION],
      requested: version ?? null,
    });
  }
  if (
    !object(meta) || !object(meta[`${META}clientInfo`]) ||
    !object(meta[`${META}clientCapabilities`])
  ) {
    return failure(id, -32602, 'Missing client metadata');
  }
  const complete = (result: ObjectValue): Reply => ({
    jsonrpc: '2.0',
    id,
    result: { resultType: 'complete', ttlMs: 0, cacheScope: 'private', ...result },
  });
  switch (message.method) {
    case 'server/discover':
      return complete({
        supportedVersions: [VERSION],
        capabilities: { tools: {}, resources: {} },
        _meta: { [`${META}serverInfo`]: { name: 'frances-dev-mcp', version: '0.1.0' } },
      });
    case 'tools/list':
      return complete({ tools });
    case 'resources/list':
      return complete({
        resources: [{
          uri: 'dev-mcp:///fixture',
          name: 'Test state',
          mimeType: 'application/json',
        }],
      });
    case 'resources/templates/list':
      return complete({ resourceTemplates: [] });
    case 'resources/read':
      if (params.uri !== 'dev-mcp:///fixture') {
        return failure(id, -32602, 'Unknown resource');
      }
      return complete({
        contents: [{
          uri: params.uri,
          mimeType: 'application/json',
          text: JSON.stringify(store.snapshot),
        }],
      });
    case 'tools/call': {
      const tool = tools.find((tool) => tool.name === params.name);
      if (!tool) return failure(id, -32602, 'Unknown tool');
      const args = params.arguments;
      if (
        !object(args) || (tool.name !== 'increment' && typeof args.text !== 'string') ||
        Object.keys(args).some((key) => !Object.hasOwn(tool.inputSchema.properties, key))
      ) {
        return failure(id, -32602, 'Invalid tool arguments');
      }
      if (tool.name === 'increment') {
        try {
          const state = await store.increment();
          return complete({
            content: [{ type: 'text', text: String(state.counter) }],
            structuredContent: state,
          });
        } catch (error) {
          console.error('State mutation failed:', error);
          return complete({
            content: [{ type: 'text', text: 'Could not persist counter' }],
            isError: true,
          });
        }
      }
      if (tool.name === 'wait') {
        const ms = args.milliseconds;
        if (typeof ms !== 'number' || !Number.isInteger(ms) || ms < 0 || ms > 60000) {
          return failure(id, -32602, 'milliseconds must be an integer between 0 and 60000');
        }
        await new Promise<void>((resolve) => {
          if (signal.aborted) return resolve();
          const finish = () => {
            clearTimeout(timer);
            signal.removeEventListener('abort', finish);
            resolve();
          };
          const timer = setTimeout(finish, ms);
          signal.addEventListener('abort', finish, { once: true });
        });
      }
      if (signal.aborted) return null;
      return complete({
        content: [{ type: 'text', text: args.text }],
        structuredContent: { text: args.text, fixture: store.snapshot.fixture },
        isError: tool.name === 'fail',
      });
    }
    default:
      return failure(id, -32601, `Unknown method: ${message.method}`);
  }
}

export async function http(request: Request, store: StateStore): Promise<Response> {
  const url = new URL(request.url);
  if (!['127.0.0.1', 'localhost'].includes(url.hostname)) {
    return new Response('Forbidden host', { status: 403 });
  }
  if (request.headers.has('origin') && request.headers.get('origin') !== url.origin) {
    return new Response('Forbidden origin', { status: 403 });
  }
  if (url.pathname !== '/mcp') return new Response('Not found', { status: 404 });
  if (request.method !== 'POST') {
    return new Response('Use POST', { status: 405, headers: { Allow: 'POST' } });
  }
  if (request.headers.get('content-type')?.split(';')[0] !== 'application/json') {
    return new Response('Use application/json', { status: 415 });
  }
  let message: unknown;
  try {
    message = await request.json();
  } catch (error) {
    console.error('Invalid JSON:', error);
    return Response.json(failure(null, -32700, 'Parse error'), { status: 400 });
  }
  if (object(message) && 'id' in message && object(message.params)) {
    const params = message.params;
    const meta = params._meta;
    const mirrors = [
      ['mcp-method', message.method],
      ['mcp-protocol-version', object(meta) ? meta[`${META}protocolVersion`] : undefined],
    ];
    if (['tools/call', 'resources/read', 'prompts/get'].includes(String(message.method))) {
      mirrors.push(['mcp-name', params.name ?? params.uri]);
    }
    for (const [header, expected] of mirrors) {
      let actual = request.headers.get(String(header));
      if (header === 'mcp-name' && actual?.startsWith('=?base64?') && actual.endsWith('?=')) {
        try {
          actual = new TextDecoder('utf-8', { fatal: true }).decode(
            Uint8Array.from(atob(actual.slice(9, -2)), (char) => char.charCodeAt(0)),
          );
        } catch (error) {
          console.error('Invalid encoded header:', error);
          actual = null;
        }
      }
      if (actual === null || actual !== expected) {
        const id = typeof message.id === 'string' || typeof message.id === 'number'
          ? message.id
          : null;
        return Response.json(failure(id, -32020, `Header mismatch: ${header}`), { status: 400 });
      }
    }
  }
  const reply = await respond(message, store, request.signal);
  if (!reply) return new Response(null, { status: 202 });
  const status = 'error' in reply ? (reply.error.code === -32601 ? 404 : 400) : 200;
  return Response.json(reply, { status });
}
