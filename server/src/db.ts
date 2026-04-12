// @ts-expect-error -- node:sqlite is experimental in Node 25
import { DatabaseSync } from "node:sqlite";
import { DB_PATH } from "./config.js";

let db: DatabaseSync;

export interface DbRow {
  [key: string]: unknown;
}

export function getDb(): DatabaseSync {
  if (!db) {
    db = new DatabaseSync(DB_PATH);
    db.exec("PRAGMA journal_mode = WAL");
    db.exec("PRAGMA foreign_keys = ON");
    initSchema(db);
  }
  return db;
}

function initSchema(db: DatabaseSync) {
  db.exec(`
    CREATE TABLE IF NOT EXISTS hook_events (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      session_id TEXT NOT NULL,
      hook_type TEXT NOT NULL,
      tool_name TEXT,
      tool_input TEXT,
      timestamp TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_hook_session ON hook_events(session_id);
    CREATE INDEX IF NOT EXISTS idx_hook_type ON hook_events(hook_type);

    CREATE TABLE IF NOT EXISTS agents (
      id TEXT PRIMARY KEY,
      session_id TEXT,
      pid INTEGER,
      parent_agent_id TEXT,
      plan_name TEXT,
      task_id TEXT,
      cwd TEXT NOT NULL,
      status TEXT NOT NULL DEFAULT 'starting',
      prompt TEXT,
      started_at TEXT NOT NULL DEFAULT (datetime('now')),
      finished_at TEXT,
      last_tool TEXT,
      last_activity_at TEXT,
      FOREIGN KEY (parent_agent_id) REFERENCES agents(id)
    );
    CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);

    CREATE TABLE IF NOT EXISTS agent_output (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      agent_id TEXT NOT NULL,
      message_type TEXT NOT NULL,
      content TEXT NOT NULL,
      timestamp TEXT NOT NULL DEFAULT (datetime('now')),
      FOREIGN KEY (agent_id) REFERENCES agents(id)
    );
    CREATE INDEX IF NOT EXISTS idx_output_agent ON agent_output(agent_id);

    CREATE TABLE IF NOT EXISTS plan_project (
      plan_name TEXT PRIMARY KEY,
      project TEXT NOT NULL,
      updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS task_status (
      plan_name TEXT NOT NULL,
      task_number TEXT NOT NULL,
      status TEXT NOT NULL DEFAULT 'pending',
      updated_at TEXT NOT NULL DEFAULT (datetime('now')),
      PRIMARY KEY (plan_name, task_number)
    );
  `);
}
