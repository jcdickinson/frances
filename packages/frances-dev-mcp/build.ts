// env -S splits shebang arguments on Unix. Pin env as well as Deno: neither
// executable should depend on the worker's PATH or an active Nix dev shell.
const env = Deno.args[0];
if (!env?.startsWith('/') || /\s/.test(env)) {
  throw new Error('Pass the absolute env executable path');
}
// Preserve the env basename: Nix coreutils may use a multicall binary.
const runtime = await Deno.realPath(Deno.execPath());
const output = new URL('../../target/frances-dev-mcp.js', import.meta.url);
const bundle = await new Deno.Command(runtime, {
  args: ['bundle', '--quiet', new URL('src/main.ts', import.meta.url).pathname],
  stdout: 'piped',
  stderr: 'inherit',
}).output();
if (!bundle.success) throw new Error('Bundling failed');
await Deno.mkdir(new URL('../../target/', import.meta.url), { recursive: true });
const shebang =
  `#!${env} -S ${runtime} run --no-config --no-lock --allow-read --allow-write --allow-net=127.0.0.1\n`;
if (new TextEncoder().encode(shebang).length > 255) throw new Error('Shebang exceeds Linux limit');
await Deno.writeTextFile(output, shebang + new TextDecoder().decode(bundle.stdout));
await Deno.chmod(output, 0o755);
console.log(output.pathname);
