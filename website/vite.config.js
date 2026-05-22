import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Hash-based routing means we can deploy at any base path without server
// rewrites — GitHub Pages serves the site at /rustygit/ when published from
// the repo's Pages tab. Override at build time with `VITE_BASE=/...`.
const base = process.env.VITE_BASE ?? "./";

export default defineConfig({
  base,
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    target: "es2020",
    cssCodeSplit: false,
    assetsInlineLimit: 4096,
    rollupOptions: {
      output: {
        // Vite 8 / Rolldown requires manualChunks as a function; group all
        // React internals into one cacheable vendor chunk so page-level
        // changes don't bust it on every build.
        manualChunks: (id) =>
          id.includes("node_modules/react") || id.includes("node_modules/scheduler")
            ? "react"
            : undefined,
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
