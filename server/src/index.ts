import express from "express";
import cors from "cors";
import { createServer } from "node:http";
import { PORT } from "./config.js";
import { getDb } from "./db.js";
import { hookRouter } from "./hooks.js";
import { apiRouter } from "./api.js";
import { initWs } from "./ws.js";
import { startFileWatcher } from "./file-watcher.js";

const app = express();
app.use(cors());
app.use(express.json());

// Routes
app.use("/hooks", hookRouter);
app.use("/api", apiRouter);

// Health check
app.get("/health", (_req, res) => {
  res.json({ status: "ok", timestamp: new Date().toISOString() });
});

// Create HTTP server and attach WebSocket
const server = createServer(app);
initWs(server);

// Initialize DB on startup
const db = getDb();

// Clean stale agents from previous runs (PIDs that no longer exist)
const staleAgents = db
  .prepare(`SELECT id, pid FROM agents WHERE status IN ('running', 'starting')`)
  .all() as { id: string; pid: number }[];
for (const a of staleAgents) {
  try {
    process.kill(a.pid, 0);
  } catch {
    db.prepare(`UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?`).run(a.id);
    console.log(`[orchestrAI] Cleaned stale agent ${a.id.slice(0, 8)} (pid ${a.pid})`);
  }
}

// Start file watcher
startFileWatcher();

server.listen(PORT, () => {
  console.log(`[orchestrAI] Server running on http://localhost:${PORT}`);
  console.log(`[orchestrAI] WebSocket at ws://localhost:${PORT}/ws`);
  console.log(`[orchestrAI] API at http://localhost:${PORT}/api`);
});
