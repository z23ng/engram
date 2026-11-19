import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vitest/config";

// `npm run dev` proxies API, MCP and WebDAV calls to a local `mem serve`.
const backend = "http://127.0.0.1:7878";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: { proxy: { "/api": backend, "/p": backend } },
  // No data: URIs: the server CSP allows fonts and scripts only from 'self'.
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 2000, assetsInlineLimit: 0 },
  test: { environment: "jsdom", setupFiles: ["./src/test/setup.ts"] },
});
