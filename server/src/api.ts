import { Router, type Request, type Response } from "express";
import path from "node:path";
import os from "node:os";
import fs from "node:fs";
import { execSync } from "node:child_process";
import { PLANS_DIR } from "./config.js";
import { listPlans, parsePlanFile } from "./plan-parser.js";
import {
  getAllAgents,
  getActiveAgents,
  getAgentOutput,
  startAgent,
  sendMessageToAgent,
  killAgent,
} from "./agent-manager.js";
import { getDb } from "./db.js";

export const apiRouter = Router();

// --- Plans ---

apiRouter.get("/plans", (_req: Request, res: Response) => {
  const plans = listPlans();
  const db = getDb();
  // Merge DB project overrides
  const overrides = db.prepare(`SELECT plan_name, project FROM plan_project`).all() as { plan_name: string; project: string }[];
  const overrideMap = new Map(overrides.map((o) => [o.plan_name, o.project]));
  // Get completed/total counts per plan
  const statusCounts = db.prepare(
    `SELECT plan_name, status, COUNT(*) as cnt FROM task_status GROUP BY plan_name, status`
  ).all() as { plan_name: string; status: string; cnt: number }[];
  const countMap = new Map<string, Record<string, number>>();
  for (const row of statusCounts) {
    if (!countMap.has(row.plan_name)) countMap.set(row.plan_name, {});
    countMap.get(row.plan_name)![row.status] = row.cnt;
  }
  const enriched = plans.map((plan) => {
    const counts = countMap.get(plan.name) ?? {};
    const doneCount = (counts.completed ?? 0) + (counts.skipped ?? 0);
    return {
      ...plan,
      project: overrideMap.get(plan.name) ?? plan.project,
      doneCount,
    };
  });
  res.json(enriched);
});

apiRouter.get("/plans/:name", (req: Request, res: Response) => {
  const filePath = path.join(PLANS_DIR, `${req.params.name}.md`);
  try {
    const plan = parsePlanFile(filePath);
    // Merge persisted task statuses
    const db = getDb();
    const statuses = db
      .prepare(`SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?`)
      .all(req.params.name) as { task_number: string; status: string; updated_at: string }[];
    const statusMap = new Map(statuses.map((s) => [s.task_number, { status: s.status, updatedAt: s.updated_at }]));
    for (const phase of plan.phases) {
      for (const task of phase.tasks) {
        const entry = statusMap.get(task.number);
        task.status = entry?.status ?? "pending";
        task.statusUpdatedAt = entry?.updatedAt;
      }
    }
    // Merge DB project override
    const projRow = db.prepare(`SELECT project FROM plan_project WHERE plan_name = ?`).get(req.params.name) as { project: string } | undefined;
    if (projRow) plan.project = projRow.project;

    res.json(plan);
  } catch {
    res.status(404).json({ error: "Plan not found" });
  }
});

// --- Plan Project ---

apiRouter.put("/plans/:name/project", (req: Request, res: Response) => {
  const { project } = req.body as { project: string };
  if (!project) {
    res.status(400).json({ error: "project is required" });
    return;
  }
  const db = getDb();
  db.prepare(
    `INSERT INTO plan_project (plan_name, project, updated_at)
     VALUES (?, ?, datetime('now'))
     ON CONFLICT(plan_name)
     DO UPDATE SET project = excluded.project, updated_at = excluded.updated_at`
  ).run(req.params.name, project);
  res.json({ ok: true, plan_name: req.params.name, project });
});

// --- Task Status ---

apiRouter.put("/plans/:name/tasks/:taskNumber/status", (req: Request, res: Response) => {
  const { name, taskNumber } = req.params;
  const { status } = req.body as { status: string };
  const validStatuses = ["pending", "in_progress", "completed", "failed", "skipped"];
  if (!validStatuses.includes(status)) {
    res.status(400).json({ error: `Invalid status. Must be one of: ${validStatuses.join(", ")}` });
    return;
  }
  const db = getDb();
  db.prepare(
    `INSERT INTO task_status (plan_name, task_number, status, updated_at)
     VALUES (?, ?, ?, datetime('now'))
     ON CONFLICT(plan_name, task_number)
     DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at`
  ).run(name, taskNumber, status);
  res.json({ ok: true, plan_name: name, task_number: taskNumber, status });
});

apiRouter.get("/plans/:name/statuses", (req: Request, res: Response) => {
  const db = getDb();
  const statuses = db
    .prepare(`SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?`)
    .all(req.params.name);
  res.json(statuses);
});

// --- Check Task (agent-based verification) ---

apiRouter.post("/plans/:name/tasks/:taskNumber/check", (req: Request, res: Response) => {
  const { name: planName, taskNumber } = req.params;
  const filePath = path.join(PLANS_DIR, `${planName}.md`);
  let plan;
  try {
    plan = parsePlanFile(filePath);
  } catch {
    res.status(404).json({ error: "Plan not found" });
    return;
  }

  if (!plan.project) {
    res.status(400).json({ error: "Plan has no associated project" });
    return;
  }

  const phase = plan.phases.find((p) => p.tasks.some((t) => t.number === taskNumber));
  const task = phase?.tasks.find((t) => t.number === taskNumber);
  if (!phase || !task) {
    res.status(404).json({ error: `Task ${taskNumber} not found` });
    return;
  }

  const projectDir = path.join(os.homedir(), plan.project);

  const prompt = [
    `You are verifying whether a task from a plan has been implemented.`,
    `Answer with ONLY a JSON object, no other text: {"status": "completed"|"in_progress"|"pending", "reason": "brief explanation"}`,
    ``,
    `Project directory: ${projectDir}`,
    `Plan: ${plan.title}`,
    `Phase ${phase.number}: ${phase.title}`,
    `Task ${task.number}: ${task.title}`,
    ``,
    `Task description:`,
    task.description,
    ``,
    task.filePaths.length > 0
      ? `Files mentioned:\n${task.filePaths.map((f) => `- ${f}`).join("\n")}`
      : "",
    task.acceptance ? `\nAcceptance criteria:\n${task.acceptance}` : "",
    ``,
    `Check the project at ${projectDir}. Read the relevant files. Determine if this task is:`,
    `- "completed": all described changes exist in the code`,
    `- "in_progress": some changes exist but the task is not fully done`,
    `- "pending": no evidence of this task being started`,
    ``,
    `Respond with ONLY the JSON object.`,
  ].filter(Boolean).join("\n");

  const agentId = startAgent({
    prompt,
    cwd: projectDir,
    planName,
    taskId: taskNumber,
    readOnly: true,
  });

  // Set task to checking state
  const db = getDb();
  db.prepare(
    `INSERT INTO task_status (plan_name, task_number, status, updated_at)
     VALUES (?, ?, 'checking', datetime('now'))
     ON CONFLICT(plan_name, task_number)
     DO UPDATE SET status = 'checking', updated_at = datetime('now')`
  ).run(planName, taskNumber);

  res.json({ agentId, planName, taskNumber });
});

// --- Auto Status Detection ---

function findFileInProject(projectDir: string, filePath: string): boolean {
  // Strip line number suffixes like :609-664
  const clean = filePath.replace(/:\d+[-–]\d+$/, "").replace(/:\d+$/, "");

  // Direct path check
  const direct = path.join(projectDir, clean);
  if (fs.existsSync(direct)) return true;

  // If it's just a filename (no directory separator), search for it
  if (!clean.includes("/") || clean.includes("...")) {
    const filename = path.basename(clean);
    try {
      const result = execSync(
        `find ${JSON.stringify(projectDir)} -name ${JSON.stringify(filename)} -not -path "*/node_modules/*" -not -path "*/.git/*" -not -path "*/target/*" 2>/dev/null | head -1`,
        { encoding: "utf-8", timeout: 5000 }
      ).trim();
      return result.length > 0;
    } catch {
      return false;
    }
  }

  return false;
}

function checkGitForTask(projectDir: string, keywords: string[]): number {
  // Check git log for commits mentioning task-related keywords
  if (!fs.existsSync(path.join(projectDir, ".git"))) return 0;

  let hits = 0;
  for (const kw of keywords) {
    if (kw.length < 4) continue;
    try {
      const result = execSync(
        `git -C ${JSON.stringify(projectDir)} log --oneline --all -5 --grep=${JSON.stringify(kw)} 2>/dev/null`,
        { encoding: "utf-8", timeout: 5000 }
      ).trim();
      if (result.length > 0) hits++;
    } catch {
      // ignore
    }
  }
  return hits;
}

apiRouter.post("/plans/:name/auto-status", (req: Request, res: Response) => {
  const planName = req.params.name;
  const filePath = path.join(PLANS_DIR, `${planName}.md`);
  let plan;
  try {
    plan = parsePlanFile(filePath);
  } catch {
    res.status(404).json({ error: "Plan not found" });
    return;
  }

  if (!plan.project) {
    res.status(400).json({ error: "Plan has no associated project. Set one via PUT /api/plans/:name/project" });
    return;
  }

  const projectDir = path.join(os.homedir(), plan.project);
  if (!fs.existsSync(projectDir)) {
    res.status(400).json({ error: `Project directory not found: ${projectDir}` });
    return;
  }

  const db = getDb();
  const results: { taskNumber: string; title: string; status: string; reason: string }[] = [];

  // Don't override manually set statuses
  const existing = db
    .prepare(`SELECT task_number, status FROM task_status WHERE plan_name = ?`)
    .all(planName) as { task_number: string; status: string }[];
  const manualStatuses = new Map(existing.map((e) => [e.task_number, e.status]));

  for (const phase of plan.phases) {
    for (const task of phase.tasks) {
      // Skip tasks that were already manually set
      const manual = manualStatuses.get(task.number);
      if (manual && manual !== "pending") {
        results.push({ taskNumber: task.number, title: task.title, status: manual, reason: "manual (kept)" });
        continue;
      }

      const filePaths = task.filePaths;
      let foundCount = 0;
      let totalChecked = 0;

      for (const fp of filePaths) {
        totalChecked++;
        if (findFileInProject(projectDir, fp)) foundCount++;
      }

      // Also check git for task title keywords
      const titleWords = task.title.split(/\s+/).filter((w) => w.length >= 5);
      const gitHits = checkGitForTask(projectDir, titleWords);

      // Determine status
      let status: string;
      let reason: string;

      if (totalChecked === 0) {
        // No files referenced — check git only
        if (gitHits >= 2) {
          status = "completed";
          reason = `${gitHits} git commits match keywords`;
        } else if (gitHits === 1) {
          status = "in_progress";
          reason = "1 git commit matches";
        } else {
          status = "pending";
          reason = "no files or git references found";
        }
      } else {
        const ratio = foundCount / totalChecked;
        if (ratio >= 0.8) {
          status = "completed";
          reason = `${foundCount}/${totalChecked} files exist`;
        } else if (ratio >= 0.3 || gitHits > 0) {
          status = "in_progress";
          reason = `${foundCount}/${totalChecked} files exist${gitHits > 0 ? `, ${gitHits} git hits` : ""}`;
        } else {
          status = "pending";
          reason = `${foundCount}/${totalChecked} files exist`;
        }
      }

      // Persist
      db.prepare(
        `INSERT INTO task_status (plan_name, task_number, status, updated_at)
         VALUES (?, ?, ?, datetime('now'))
         ON CONFLICT(plan_name, task_number)
         DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at`
      ).run(planName, task.number, status);

      results.push({ taskNumber: task.number, title: task.title, status, reason });
    }
  }

  res.json({
    plan: planName,
    project: plan.project,
    projectDir,
    results,
    summary: {
      total: results.length,
      completed: results.filter((r) => r.status === "completed").length,
      in_progress: results.filter((r) => r.status === "in_progress").length,
      pending: results.filter((r) => r.status === "pending").length,
    },
  });
});

apiRouter.post("/plans/sync-all", (_req: Request, res: Response) => {
  const plans = listPlans().filter((p) => p.project);
  const db = getDb();
  const totals = { completed: 0, in_progress: 0, pending: 0 };

  for (const planSummary of plans) {
    const filePath = path.join(PLANS_DIR, `${planSummary.name}.md`);
    let plan;
    try {
      plan = parsePlanFile(filePath);
    } catch {
      continue;
    }
    if (!plan.project) continue;

    const projectDir = path.join(os.homedir(), plan.project);
    if (!fs.existsSync(projectDir)) continue;

    // Check DB overrides
    const existing = db
      .prepare(`SELECT task_number, status FROM task_status WHERE plan_name = ?`)
      .all(planSummary.name) as { task_number: string; status: string }[];
    const manualStatuses = new Map(existing.map((e) => [e.task_number, e.status]));

    for (const phase of plan.phases) {
      for (const task of phase.tasks) {
        const manual = manualStatuses.get(task.number);
        if (manual && manual !== "pending") {
          totals[manual as keyof typeof totals] = (totals[manual as keyof typeof totals] ?? 0) + 1;
          continue;
        }

        const filePaths = task.filePaths;
        let foundCount = 0;
        let totalChecked = 0;
        for (const fp of filePaths) {
          totalChecked++;
          if (findFileInProject(projectDir, fp)) foundCount++;
        }

        const titleWords = task.title.split(/\s+/).filter((w) => w.length >= 5);
        const gitHits = checkGitForTask(projectDir, titleWords);

        let status: string;
        if (totalChecked === 0) {
          status = gitHits >= 2 ? "completed" : gitHits === 1 ? "in_progress" : "pending";
        } else {
          const ratio = foundCount / totalChecked;
          status = ratio >= 0.8 ? "completed" : ratio >= 0.3 || gitHits > 0 ? "in_progress" : "pending";
        }

        db.prepare(
          `INSERT INTO task_status (plan_name, task_number, status, updated_at)
           VALUES (?, ?, ?, datetime('now'))
           ON CONFLICT(plan_name, task_number)
           DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at`
        ).run(planSummary.name, task.number, status);

        totals[status as keyof typeof totals] = (totals[status as keyof typeof totals] ?? 0) + 1;
      }
    }
  }

  res.json({ synced: plans.length, ...totals });
});

// --- Agents ---

apiRouter.get("/agents", (_req: Request, res: Response) => {
  res.json(getAllAgents());
});

apiRouter.get("/agents/active", (_req: Request, res: Response) => {
  res.json(getActiveAgents());
});

apiRouter.get("/agents/:id/output", (req: Request, res: Response) => {
  const limit = Number(req.query.limit ?? 200);
  const offset = Number(req.query.offset ?? 0);
  res.json(getAgentOutput(req.params.id, limit, offset));
});

apiRouter.post("/agents/:id/message", (req: Request, res: Response) => {
  const { message } = req.body as { message: string };
  if (!message) {
    res.status(400).json({ error: "message is required" });
    return;
  }
  const ok = sendMessageToAgent(req.params.id, message);
  if (!ok) {
    res.status(404).json({ error: "Agent not found or not running" });
    return;
  }
  res.json({ ok: true });
});

apiRouter.delete("/agents/:id", (req: Request, res: Response) => {
  const ok = killAgent(req.params.id);
  if (!ok) {
    res.status(404).json({ error: "Agent not found" });
    return;
  }
  res.json({ ok: true });
});

// --- Actions ---

apiRouter.post("/actions/start-task", (req: Request, res: Response) => {
  const { planName, phaseNumber, taskNumber, cwd, mode } = req.body as {
    planName: string;
    phaseNumber: number;
    taskNumber: string;
    cwd?: string;
    mode?: "start" | "continue";
  };

  if (!planName || phaseNumber === undefined || !taskNumber) {
    res.status(400).json({ error: "planName, phaseNumber, and taskNumber are required" });
    return;
  }

  const filePath = path.join(PLANS_DIR, `${planName}.md`);
  let plan;
  try {
    plan = parsePlanFile(filePath);
  } catch {
    res.status(404).json({ error: "Plan not found" });
    return;
  }

  const phase = plan.phases.find((p) => p.number === phaseNumber);
  if (!phase) {
    res.status(404).json({ error: `Phase ${phaseNumber} not found` });
    return;
  }

  const task = phase.tasks.find((t) => t.number === taskNumber);
  if (!task) {
    res.status(404).json({ error: `Task ${taskNumber} not found in phase ${phaseNumber}` });
    return;
  }

  const isContinue = mode === "continue";
  const prompt = [
    isContinue
      ? `You are continuing work on a partially implemented task. Some parts may already exist — check the current state of the code before making changes.`
      : `You are working on the following task from a plan.`,
    ``,
    `Plan: ${plan.title}`,
    `Phase ${phase.number}: ${phase.title}`,
    `Task ${task.number}: ${task.title}`,
    ``,
    `Description:`,
    task.description,
    ``,
    task.filePaths.length > 0
      ? `Files involved:\n${task.filePaths.map((f) => `- ${f}`).join("\n")}`
      : "",
    task.acceptance ? `\nAcceptance criteria:\n${task.acceptance}` : "",
    ``,
    isContinue
      ? `First, read the relevant files to understand what has already been done. Then complete the remaining work. When done, summarize what was already in place and what you changed.`
      : `Please implement this task. When done, summarize what you changed.`,
    ``,
    `IMPORTANT: When you are finished, you MUST end your final message with a JSON status on its own line:`,
    `{"status": "completed", "reason": "brief summary of what was done"}`,
    `If you could not fully complete the task, use:`,
    `{"status": "in_progress", "reason": "what remains to be done"}`,
  ]
    .filter(Boolean)
    .join("\n");

  // Use explicit cwd, or resolve from plan's project, or fall back to process.cwd()
  const workDir = cwd ?? (plan.project ? path.join(os.homedir(), plan.project) : process.cwd());
  const agentId = startAgent({
    prompt,
    cwd: workDir,
    planName,
    taskId: `${phaseNumber}.${taskNumber}`,
  });

  res.json({ agentId, taskId: `${phaseNumber}.${taskNumber}` });
});

// --- Hook Events ---

apiRouter.get("/events", (req: Request, res: Response) => {
  const limit = Number(req.query.limit ?? 50);
  const db = getDb();
  const events = db
    .prepare(`SELECT * FROM hook_events ORDER BY id DESC LIMIT ?`)
    .all(limit);
  res.json(events);
});

apiRouter.get("/events/:sessionId", (req: Request, res: Response) => {
  const limit = Number(req.query.limit ?? 100);
  const db = getDb();
  const events = db
    .prepare(
      `SELECT * FROM hook_events WHERE session_id = ? ORDER BY id DESC LIMIT ?`
    )
    .all(req.params.sessionId, limit);
  res.json(events);
});
