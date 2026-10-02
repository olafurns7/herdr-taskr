import { defineConfig } from "vitest/config";

// The page is served by the taskr daemon under a strict CSP (script-src,
// style-src and font-src 'self'): no inline script or style, no data: fonts.
export default defineConfig({
  oxc: { jsx: { runtime: "automatic", importSource: "preact" } },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    modulePreload: { polyfill: false },
    sourcemap: false,
    target: "es2022",
    // Preact's runtime keeps its dangerouslySetInnerHTML path (this app never
    // passes that prop); it ships as its own chunk so the Go test can hold
    // every app chunk to zero HTML sinks.
    rolldownOptions: {
      output: {
        entryFileNames: "assets/app-[hash].js",
        codeSplitting: { groups: [{ name: "preact", test: /[\\/]node_modules[\\/]preact[\\/]/ }] },
      },
    },
  },
  server: {
    // `pnpm dev`: the daemon's API through a proxy. Answering needs the
    // daemon's own page (its token and same-origin checks stay).
    proxy: { "/api": { target: "http://127.0.0.1:7788", changeOrigin: true } },
  },
  test: { include: ["src/**/*.test.ts"] },
});
