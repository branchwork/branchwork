import path from "node:path";
import os from "node:os";

export const PORT = Number(process.env.PORT ?? 3100);
export const CLAUDE_DIR = path.join(os.homedir(), ".claude");
export const PLANS_DIR = path.join(CLAUDE_DIR, "plans");
export const TASKS_DIR = path.join(CLAUDE_DIR, "tasks");
export const SESSIONS_DIR = path.join(CLAUDE_DIR, "sessions");
export const DB_PATH = path.join(CLAUDE_DIR, "orchestrai.db");
