import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

// Builds the Automations editor as a single-file ES bundle that the Rust
// server can include_bytes!() and serve at /assets/automations.js. The
// resulting bundle attaches itself to window.FuseboxAutomations and is
// loaded lazily by the main page when the Automations tab is opened.
export default defineConfig({
  plugins: [react(), viteSingleFile()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2020",
    cssCodeSplit: false,
    assetsInlineLimit: 1024 * 1024,
    rollupOptions: {
      input: "src/main.tsx",
      output: {
        format: "iife",
        entryFileNames: "automations.js",
        inlineDynamicImports: true,
        name: "FuseboxAutomations",
      },
    },
  },
});
