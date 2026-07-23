import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Electron loads the built app from disk. Use relative asset URLs and load the
// page over a custom app:// origin (see electron/main.mjs) so ES module scripts
// are not blocked by the file:// null-origin CORS policy.
export default defineConfig({
  base: "./",
  plugins: [svelte()],
});
