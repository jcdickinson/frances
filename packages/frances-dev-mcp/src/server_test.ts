import { deepStrictEqual, equal, rejects } from 'node:assert/strict';
import { http, META, respond, VERSION } from './server.ts';
import { StateStore } from './state.ts';

function request(method: string, params: Record<string, unknown> = {}, id = 1) {
  return {
    jsonrpc: '2.0',
    id,
    method,
    params: {
      ...params,
      _meta: {
        [`${META}protocolVersion`]: VERSION,
        [`${META}clientInfo`]: { name: 'test', version: '1' },
        [`${META}clientCapabilities`]: {},
      },
    },
  };
}

Deno.test('discovery, tool failures, invalid arguments, and resource state', async () => {
  const store = await StateStore.open();
  const discover = await respond(request('server/discover'), store);
  equal(discover && 'result' in discover && discover.result.resultType, 'complete');
  const failed = await respond(
    request('tools/call', { name: 'fail', arguments: { text: 'failure' } }),
    store,
  );
  equal(failed && 'result' in failed && failed.result.isError, true);
  const invalid = await respond(
    request('tools/call', { name: 'wait', arguments: { text: 'x', milliseconds: -1 } }),
    store,
  );
  equal(invalid && 'error' in invalid && invalid.error.code, -32602);
  const resource = await respond(request('resources/read', { uri: 'dev-mcp:///fixture' }), store);
  deepStrictEqual(resource && 'result' in resource && resource.result.contents, [{
    uri: 'dev-mcp:///fixture',
    mimeType: 'application/json',
    text: '{"counter":0,"fixture":{}}',
  }]);
});

Deno.test('concurrent mutations survive reopening; corrupt state fails explicitly', async () => {
  const dir = await Deno.makeTempDir();
  try {
    const path = `${dir}/state.json`;
    const store = await StateStore.open(path);
    await Promise.all(Array.from({ length: 20 }, () => store.increment()));
    equal(store.snapshot.counter, 20);
    equal((await StateStore.open(path)).snapshot.counter, 20);
    const entries = [];
    for await (const entry of Deno.readDir(dir)) entries.push(entry.name);
    deepStrictEqual(entries, ['state.json']);
    await Deno.writeTextFile(path, 'broken');
    await rejects(() => StateStore.open(path));
    equal(await Deno.readTextFile(path), 'broken');
  } finally {
    await Deno.remove(dir, { recursive: true });
  }
});

Deno.test('failed persistence does not advance the in-memory counter', async () => {
  const dir = await Deno.makeTempDir();
  const store = await StateStore.open(`${dir}/state.json`);
  await Deno.remove(dir, { recursive: true });
  await rejects(() => store.increment());
  equal(store.snapshot.counter, 0);
});

Deno.test('HTTP validates routing, origin, metadata and serves real requests', async () => {
  const store = await StateStore.open();
  const server = Deno.serve(
    { hostname: '127.0.0.1', port: 0, onListen() {} },
    (req) => http(req, store),
  );
  const url = `http://127.0.0.1:${server.addr.port}/mcp`;
  try {
    const message = request('tools/call', { name: 'echo', arguments: { text: 'hello' } });
    const headers = {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      'mcp-protocol-version': VERSION,
      'mcp-method': 'tools/call',
      'mcp-name': 'echo',
    };
    const response = await fetch(url, { method: 'POST', headers, body: JSON.stringify(message) });
    equal(response.status, 200);
    equal((await response.json()).result.content[0].text, 'hello');
    for (
      const [patch, status] of [[{ origin: 'https://evil.example' }, 403], [
        { 'mcp-name': 'fail' },
        400,
      ]] as const
    ) {
      const response = await fetch(url, {
        method: 'POST',
        headers: { ...headers, ...patch },
        body: JSON.stringify(message),
      });
      equal(response.status, status);
      await response.arrayBuffer();
    }
    const get = await fetch(url);
    equal(get.status, 405);
    await get.arrayBuffer();
  } finally {
    await server.shutdown();
  }
});

Deno.test('stdio cancellation does not block another request; startup reloads state', async () => {
  const dir = await Deno.makeTempDir();
  const state = `${dir}/state.json`;
  await Deno.writeTextFile(state, '{"counter":9,"fixture":{"marker":"saved"}}');
  const process = new Deno.Command(Deno.execPath(), {
    args: [
      'run',
      '--no-config',
      '--allow-read',
      '--allow-write',
      new URL('./main.ts', import.meta.url).pathname,
      '--state',
      state,
    ],
    stdin: 'piped',
    stdout: 'piped',
    stderr: 'piped',
    clearEnv: true,
    env: { PATH: '/missing' },
    cwd: dir,
  }).spawn();
  const writer = process.stdin.getWriter();
  const reader = process.stdout.pipeThrough(new TextDecoderStream()).getReader();
  const send = (value: unknown) =>
    writer.write(new TextEncoder().encode(JSON.stringify(value) + '\n'));
  let buffer = '';
  const next = async () => {
    while (!buffer.includes('\n')) {
      const chunk = await reader.read();
      if (chunk.done) throw new Error('Unexpected EOF');
      buffer += chunk.value;
    }
    const end = buffer.indexOf('\n');
    const value = JSON.parse(buffer.slice(0, end));
    buffer = buffer.slice(end + 1);
    return value;
  };
  try {
    await send(
      request('tools/call', { name: 'wait', arguments: { text: 'slow', milliseconds: 60000 } }, 1),
    );
    await send({ jsonrpc: '2.0', method: 'notifications/cancelled', params: { requestId: 1 } });
    await send(request('tools/call', { name: 'increment', arguments: {} }, 2));
    const reply = await next();
    equal(reply.id, 2);
    equal(reply.result.structuredContent.counter, 10);
    equal((await StateStore.open(state)).snapshot.counter, 10);
  } finally {
    await writer.close();
    await reader.cancel();
    await process.stderr.cancel();
    await process.status;
    await Deno.remove(dir, { recursive: true });
  }
});
