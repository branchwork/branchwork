import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import { getDb } from "./db.js";
import { broadcast } from "./ws.js";

interface ManagedAgent {
  id: string;
  process: ChildProcess;
  sessionId: string;
  planName?: string;
  taskId?: string;
}

const agents = new Map<string, ManagedAgent>();

export function getActiveAgents() {
  const db = getDb();
  return db
    .prepare(`SELECT * FROM agents WHERE status IN ('starting', 'running') ORDER BY started_at DESC`)
    .all();
}

export function getAllAgents() {
  const db = getDb();
  return db.prepare(`SELECT * FROM agents ORDER BY started_at DESC`).all();
}

export function getAgentOutput(agentId: string, limit = 200, offset = 0) {
  const db = getDb();
  return db
    .prepare(
      `SELECT * FROM agent_output WHERE agent_id = ? ORDER BY id ASC LIMIT ? OFFSET ?`
    )
    .all(agentId, limit, offset);
}

export function startAgent(opts: {
  prompt: string;
  cwd: string;
  planName?: string;
  taskId?: string;
  parentAgentId?: string;
  readOnly?: boolean;
}): string {
  const id = randomUUID();
  const sessionId = randomUUID();
  const db = getDb();

  db.prepare(
    `INSERT INTO agents (id, session_id, cwd, status, plan_name, task_id, parent_agent_id, prompt)
     VALUES (?, ?, ?, 'starting', ?, ?, ?, ?)`
  ).run(id, sessionId, opts.cwd, opts.planName ?? null, opts.taskId ?? null, opts.parentAgentId ?? null, opts.prompt);

  const args = [
    "-p",
    "--verbose",
    "--output-format",
    "stream-json",
    "--input-format",
    "stream-json",
    "--session-id",
    sessionId,
    "--add-dir",
    opts.cwd,
  ];

  if (opts.readOnly) {
    args.push("--permission-mode", "plan", "--allowedTools", "Read,Glob,Grep,Bash(git:*)");
  }

  const child = spawn(
    "claude",
    args,
    {
      cwd: opts.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env },
    }
  );

  // Send initial prompt as stream-json user message
  const initMsg = JSON.stringify({
    type: "user",
    message: {
      role: "user",
      content: [{ type: "text", text: opts.prompt }],
    },
  });
  child.stdin!.write(initMsg + "\n");

  if (opts.readOnly) {
    // Check agents don't need further input
    child.stdin!.end();
  }

  const agent: ManagedAgent = { id, process: child, sessionId, planName: opts.planName, taskId: opts.taskId };
  agents.set(id, agent);

  db.prepare(`UPDATE agents SET pid = ?, status = 'running' WHERE id = ?`).run(
    child.pid,
    id
  );

  broadcast("agent_started", { id, sessionId, planName: opts.planName, taskId: opts.taskId, pid: child.pid });

  // Stream stdout line by line
  const rl = createInterface({ input: child.stdout! });
  rl.on("line", (line) => {
    try {
      const parsed = JSON.parse(line);
      const msgType = parsed.type ?? "unknown";

      db.prepare(
        `INSERT INTO agent_output (agent_id, message_type, content) VALUES (?, ?, ?)`
      ).run(id, msgType, line);

      broadcast("agent_output", { agent_id: id, message_type: msgType, content: parsed });

      // When we receive a "result" event, the agent turn is done — close stdin
      // so the process exits and the exit handler can parse the verdict
      if (parsed.type === "result" && child.stdin && !child.stdin.destroyed) {
        child.stdin.end();
      }
    } catch {
      // Non-JSON line
      db.prepare(
        `INSERT INTO agent_output (agent_id, message_type, content) VALUES (?, ?, ?)`
      ).run(id, "raw", line);
    }
  });

  // Capture stderr
  const stderrRl = createInterface({ input: child.stderr! });
  stderrRl.on("line", (line) => {
    db.prepare(
      `INSERT INTO agent_output (agent_id, message_type, content) VALUES (?, ?, ?)`
    ).run(id, "stderr", line);
  });

  child.on("exit", (code) => {
    const agentStatus = code === 0 ? "completed" : "failed";
    db.prepare(
      `UPDATE agents SET status = ?, finished_at = datetime('now') WHERE id = ?`
    ).run(agentStatus, id);
    agents.delete(id);

    // Parse output for task status verdict (check, start, and continue agents)
    if (opts.taskId) {
      try {
        const outputRows = db
          .prepare(`SELECT content FROM agent_output WHERE agent_id = ? ORDER BY id`)
          .all(id) as { content: string }[];

        // Look for a JSON verdict in the output (scan from end)
        let verdictFound = false;
        for (const row of outputRows.reverse()) {
          try {
            const outer = JSON.parse(row.content);
            // stream-json wraps in {type: "result", result: "..."} or {type: "assistant", message: {content: [...]}}
            let text = "";
            if (outer.result) {
              text = outer.result;
            } else if (outer.message?.content) {
              for (const block of outer.message.content) {
                if (block.type === "text") text += block.text;
              }
            } else if (outer.type === "text") {
              text = outer.text ?? "";
            }
            // Extract JSON verdict from the text
            const jsonMatch = text.match(/\{\s*"status"\s*:\s*"(completed|in_progress|pending)"[^}]*\}/);
            if (jsonMatch) {
              try {
                const verdict = JSON.parse(jsonMatch[0]);
                db.prepare(
                  `INSERT INTO task_status (plan_name, task_number, status, updated_at)
                   VALUES (?, ?, ?, datetime('now'))
                   ON CONFLICT(plan_name, task_number)
                   DO UPDATE SET status = excluded.status, updated_at = datetime('now')`
                ).run(opts.planName, opts.taskId, verdict.status);

                broadcast("task_checked", {
                  plan_name: opts.planName,
                  task_number: opts.taskId,
                  status: verdict.status,
                  reason: verdict.reason ?? "",
                  agent_id: id,
                });
                verdictFound = true;
              } catch {
                // malformed JSON, continue scanning
              }
            }
            if (verdictFound) break;
          } catch {
            // not valid JSON, skip
          }
        }

        // Fallback: if agent exited successfully but didn't output a verdict,
        // mark the task as completed (it did its job without reporting)
        if (!verdictFound && code === 0 && !opts.readOnly) {
          db.prepare(
            `INSERT INTO task_status (plan_name, task_number, status, updated_at)
             VALUES (?, ?, 'completed', datetime('now'))
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = 'completed', updated_at = datetime('now')`
          ).run(opts.planName, opts.taskId);

          broadcast("task_checked", {
            plan_name: opts.planName,
            task_number: opts.taskId,
            status: "completed",
            reason: "Agent finished successfully",
            agent_id: id,
          });
        }
      } catch (e) {
        console.error(`[agent-manager] Failed to parse check result for agent ${id}:`, e);
      }
    }

    broadcast("agent_stopped", { id, status: agentStatus, exit_code: code });
  });

  return id;
}

export function sendMessageToAgent(agentId: string, message: string): boolean {
  const agent = agents.get(agentId);
  if (!agent || !agent.process.stdin?.writable) return false;

  const jsonMsg = JSON.stringify({
    type: "user",
    message: {
      role: "user",
      content: [{ type: "text", text: message }],
    },
  });
  agent.process.stdin.write(jsonMsg + "\n");
  return true;
}

export function killAgent(agentId: string): boolean {
  const agent = agents.get(agentId);
  if (!agent) return false;

  agent.process.kill("SIGTERM");
  const db = getDb();
  db.prepare(
    `UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?`
  ).run(agentId);
  agents.delete(agentId);
  broadcast("agent_stopped", { id: agentId, status: "killed" });
  return true;
}
