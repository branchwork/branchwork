import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const backendPort = process.env.BRANCHWORK_BACKEND_PORT ?? "3100";
const backendHttp = `http://localhost:${backendPort}`;
const backendWs = `ws://localhost:${backendPort}`;

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": backendHttp,
      "/hooks": backendHttp,
      "/ws": {
        target: backendWs,
        ws: true,
      },
      "/terminal": {
        target: backendWs,
        ws: true,
      },
    },
  },
});
