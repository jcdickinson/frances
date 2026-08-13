import { svelte } from '@sveltejs/vite-plugin-svelte';
import process from 'node:process';
import { defineConfig } from 'vite';

export default defineConfig({
  root: new URL('.', import.meta.url).pathname,
  clearScreen: false,
  plugins: [svelte()],
  server: {
    host: process.env.TAURI_DEV_HOST ?? false,
    port: 5173,
    strictPort: true,
  },
});
