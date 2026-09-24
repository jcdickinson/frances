<script lang="ts">
  import * as smd from 'streaming-markdown';

  let { text, done }: { text: string; done: boolean } = $props();

  let root: HTMLElement;
  let parser: ReturnType<typeof smd.parser> | undefined;
  let written = '';
  let ended = false;

  // The producer refreshes the full text on every delta, so feed the
  // parser only the suffix beyond what it has already consumed. If the
  // text stops being an extension of what was written (or arrives after
  // the parser was ended), start over from an empty root.
  $effect(() => {
    if (parser === undefined || ended || !text.startsWith(written)) {
      root.replaceChildren();
      parser = smd.parser(smd.default_renderer(root));
      written = '';
      ended = false;
    }
    if (text.length > written.length) {
      smd.parser_write(parser, text.slice(written.length));
      written = text;
    }
    if (done) {
      smd.parser_end(parser);
      ended = true;
    }
  });
</script>

<div class="markdown" bind:this={root}></div>
