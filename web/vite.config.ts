import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The browser surface's build.
//
// **Output lands in the crate, not in `dist/` beside this file.** The Rust build
// embeds it with `include_str!`/`include_bytes!`, which keeps the container's
// runtime stage what it has always been — one static binary, no asset directory
// to mount and no file that can go missing. A `dist/` the server read from disk
// would undo that.
export default defineConfig({
  // Served from /ui/, so the built document references /ui/app.js rather
  // than /app.js — the server owns / and every other path on this origin.
  base: "/ui/",
  plugins: [react()],
  build: {
    outDir: "../crates/sc-server/assets/ui",
    emptyOutDir: true,
    // **Predictable filenames, no content hash.** Hashing is for cache-busting
    // behind a CDN; every response from this server carries `Cache-Control:
    // no-store` without exception, so there is nothing to bust. What hashing
    // would cost is a build step that rewrites the `include_str!` paths on every
    // change, which is machinery in service of nothing.
    rollupOptions: {
      output: {
        entryFileNames: "app.js",
        chunkFileNames: "app-[name].js",
        assetFileNames: "app.[ext]",
      },
    },
    // Inlining would produce a `data:` URI, and the CSP has no `data:` in
    // `img-src` — deliberately. Anything that would be inlined must be a file.
    assetsInlineLimit: 0,
    sourcemap: false,
  },
  server: {
    port: 5173,
    // The dev loop: this serves the interface with hot reload, and anything it
    // does not own goes to a debug `sc-server`. Both halves stay in their own
    // build, and neither needs the other rebuilt to iterate.
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8791",
        changeOrigin: false,
      },
      "/public": { target: "http://127.0.0.1:8791", changeOrigin: false },
    },
  },
});
