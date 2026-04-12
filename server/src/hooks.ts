import { Router, type Request, type Response } from "express";
import { getDb } from "./db.js";
import { broadcast } from "./ws.js";

export const hookRouter = Router();

interface HookEvent {
  session_id?: string;
  hook_event_name?: string;
  hook_type?: string;
  tool_name?: string;
  tool_input?: Record<string, unknown>;
  [key: string]: unknown;
}

hookRouter.post("/", (req: Request, res: Response) => {
  const event = req.body as HookEvent;
  const sessionId = event.session_id ?? "unknown";
  const hookType = event.hook_event_name ?? event.hook_type ?? "unknown";
  const toolName = event.tool_name ?? null;
  const toolInput = event.tool_input ? JSON.stringify(event.tool_input) : null;

  const db = getDb();
  db.prepare(
    `INSERT INTO hook_events (session_id, hook_type, tool_name, tool_input) VALUES (?, ?, ?, ?)`
  ).run(sessionId, hookType, toolName, toolInput);

  // Update agent last_tool if we track this session
  if (toolName) {
    db.prepare(
      `UPDATE agents SET last_tool = ?, last_activity_at = datetime('now') WHERE session_id = ? AND status IN ('starting', 'running')`
    ).run(toolName, sessionId);
  }

  broadcast("hook_event", {
    session_id: sessionId,
    hook_type: hookType,
    tool_name: toolName,
    tool_input: event.tool_input,
  });

  res.status(200).json({ ok: true });
});
