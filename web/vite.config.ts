import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Relative base so the bundle can be embedded and served from any sub-path.
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8787",
      "/ws": {
        target: "ws://127.0.0.1:8787",
        ws: true,
      },
    },
  },
  build: {
    target: "es2022",
    sourcemap: false,
  },
});
