import { defineConfig } from "vite";

// Electron loads the built app from disk. Use relative asset URLs and load the
// page over a custom app:// origin (see electron/main.mjs) so ES module scripts
// are not blocked by the file:// null-origin CORS policy.
export default defineConfig({
  base: "./",
});
