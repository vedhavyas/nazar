import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [svelte()],
  // The vendored go2rtc client lives in static/ and is served at /.
  publicDir: 'static',
});
