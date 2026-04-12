import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { PLANS_DIR } from "./config.js";

export interface PlanTask {
  number: string;
  title: string;
  description: string;
  filePaths: string[];
  acceptance: string;
  status?: string;
  statusUpdatedAt?: string;
}

export interface PlanPhase {
  number: number;
  title: string;
  description: string;
  tasks: PlanTask[];
}

export interface ParsedPlan {
  name: string;
  filePath: string;
  title: string;
  context: string;
  project: string | null;
  createdAt: string;
  modifiedAt: string;
  phases: PlanPhase[];
}

export interface PlanSummary {
  name: string;
  title: string;
  project: string | null;
  phaseCount: number;
  taskCount: number;
  createdAt: string;
  modifiedAt: string;
}

// Extract file paths from text — matches backtick-wrapped paths and bare paths
const FILE_PATH_RE =
  /`([a-zA-Z0-9_./-]+\.[a-zA-Z0-9]+(?::\d+[-–]\d+)?)`|(?:^|\s)((?:\/[\w.-]+){2,}(?:\.\w+)?(?::\d+[-–]\d+)?)/gm;

function extractFilePaths(text: string): string[] {
  const paths: string[] = [];
  let m: RegExpExecArray | null;
  FILE_PATH_RE.lastIndex = 0;
  while ((m = FILE_PATH_RE.exec(text)) !== null) {
    paths.push(m[1] ?? m[2]);
  }
  return [...new Set(paths)];
}

// Known project directories — scanned once at startup
let _projectDirs: string[] | null = null;

function getProjectDirs(): string[] {
  if (_projectDirs) return _projectDirs;
  const homeDir = os.homedir();
  try {
    _projectDirs = fs.readdirSync(homeDir, { withFileTypes: true })
      .filter((d) => d.isDirectory() && !d.name.startsWith("."))
      .map((d) => path.join(homeDir, d.name));
  } catch {
    _projectDirs = [];
  }
  return _projectDirs;
}

function inferProject(raw: string): string | null {
  // 1. Look for absolute paths like /home/user/project-name/
  const absPathRe = /\/home\/\w+\/([\w.-]+)\//g;
  const absCounts = new Map<string, number>();
  let m: RegExpExecArray | null;
  while ((m = absPathRe.exec(raw)) !== null) {
    const proj = m[1];
    absCounts.set(proj, (absCounts.get(proj) ?? 0) + 1);
  }
  if (absCounts.size > 0) {
    // Return the most frequently referenced project
    const sorted = [...absCounts.entries()].sort((a, b) => b[1] - a[1]);
    const candidate = sorted[0][0];
    // Verify it's a real directory
    const fullPath = path.join(os.homedir(), candidate);
    if (fs.existsSync(fullPath)) return candidate;
  }

  // 2. Look for known project markers in relative paths
  const projectDirs = getProjectDirs();
  const projectNames = projectDirs.map((d) => path.basename(d));

  // Check for crates/ directories → likely a Rust project
  const crateRe = /crates\/([\w-]+)/g;
  const crateNames = new Set<string>();
  while ((m = crateRe.exec(raw)) !== null) {
    crateNames.add(m[1]);
  }

  // Match crate names to project dirs
  for (const crate of crateNames) {
    for (const projName of projectNames) {
      // e.g., crate "varpulis-sase" → project "cep" (contains crates/varpulis-sase)
      const projPath = path.join(os.homedir(), projName, "crates", crate);
      if (fs.existsSync(projPath)) return projName;
    }
  }

  // 3. Look for Java-style module names (varpulis-iot-api → varpulis-iot project)
  const moduleRe = /\b([\w]+-[\w]+-[\w]+|[\w]+-[\w]+)\/src\//g;
  while ((m = moduleRe.exec(raw)) !== null) {
    const moduleName = m[1];
    // Check if a parent project dir contains this module
    for (const projName of projectNames) {
      const modulePath = path.join(os.homedir(), projName, moduleName);
      if (fs.existsSync(modulePath)) return projName;
    }
  }

  // 4. Match project name mentioned in title or context (first 500 chars)
  const header = raw.slice(0, 500).toLowerCase();
  for (const projName of projectNames) {
    if (projName.length >= 4 && header.includes(projName.toLowerCase())) {
      return projName;
    }
  }

  return null;
}

export function parsePlanMarkdown(raw: string, name: string, filePath: string): ParsedPlan {
  const lines = raw.split("\n");

  // Title: first # heading
  let title = name;
  for (const line of lines) {
    if (/^# /.test(line)) {
      title = line.replace(/^# /, "").trim();
      break;
    }
  }

  // Split into sections by ## headings
  interface Section {
    heading: string;
    body: string[];
  }
  const sections: Section[] = [];
  let current: Section | null = null;

  for (const line of lines) {
    if (/^## /.test(line)) {
      if (current) sections.push(current);
      current = { heading: line.replace(/^## /, "").trim(), body: [] };
    } else if (current) {
      current.body.push(line);
    }
  }
  if (current) sections.push(current);

  // Extract context section
  let context = "";
  const contextSection = sections.find(
    (s) => /^context/i.test(s.heading)
  );
  if (contextSection) {
    context = contextSection.body.join("\n").trim();
  }

  // Identify phase sections — match multiple naming conventions
  const phaseRe =
    /^(?:Phase|Step)\s+(\d+\w?)[:\s.—-]+(.+)/i;
  const numberedRe = /^(\d+)[\.)]\s+(.+)/;

  const phases: PlanPhase[] = [];

  for (const section of sections) {
    let phaseNum: number | null = null;
    let phaseTitle = "";

    let m = phaseRe.exec(section.heading);
    if (m) {
      phaseNum = parseInt(m[1], 10);
      phaseTitle = m[2].trim();
    } else {
      m = numberedRe.exec(section.heading);
      if (m) {
        phaseNum = parseInt(m[1], 10);
        phaseTitle = m[2].trim();
      }
    }

    // Fallback: treat "Changes", "Implementation", etc. as phase 0
    if (phaseNum === null) {
      const implRe = /^(changes|implementation|approach|design|the change)/i;
      if (implRe.test(section.heading)) {
        phaseNum = phases.length;
        phaseTitle = section.heading;
      }
    }

    // Skip non-phase sections
    if (phaseNum === null) continue;

    // Parse tasks within the phase — ### sub-headings
    const tasks: PlanTask[] = [];
    const body = section.body.join("\n");

    // Split by ### headings
    const taskSections = body.split(/(?=^### )/m);

    for (const taskBlock of taskSections) {
      // Match ### N.M Title, ### N. Title, or ### Title
      const taskHeadingMatch =
        /^### (\d+[\.\d]*\w?)\s+(.+)/.exec(taskBlock) ??
        /^### (\S+)\s+(.+)/.exec(taskBlock);
      if (!taskHeadingMatch) continue;

      const taskNumber = taskHeadingMatch[1].replace(/\.$/, "");
      const taskTitle = taskHeadingMatch[2].replace(/^[—:-]+\s*/, "").trim();
      const taskBody = taskBlock
        .split("\n")
        .slice(1)
        .join("\n")
        .trim();

      // Extract acceptance criteria
      let acceptance = "";
      const accMatch =
        /\*\*Acceptance:?\*\*\s*(.+?)(?=\n\*\*|\n###|\n---|\n$)/is.exec(
          taskBody
        );
      if (accMatch) {
        acceptance = accMatch[1].trim();
      }

      // Extract file paths
      const filePaths = extractFilePaths(taskBody);

      tasks.push({
        number: taskNumber,
        title: taskTitle,
        description: taskBody,
        filePaths,
        acceptance,
      });
    }

    // If no ### tasks found, try bullet points with bold titles
    if (tasks.length === 0) {
      const bulletRe = /^[-*]\s+\*\*(.+?)\*\*\s*[—:-]?\s*(.*)/gm;
      let bm: RegExpExecArray | null;
      let idx = 1;
      bulletRe.lastIndex = 0;
      while ((bm = bulletRe.exec(body)) !== null) {
        tasks.push({
          number: `${phaseNum}.${idx}`,
          title: bm[1].trim(),
          description: bm[2]?.trim() ?? "",
          filePaths: extractFilePaths(bm[0]),
          acceptance: "",
        });
        idx++;
      }
    }

    // Last resort: if still no tasks, treat entire phase body as one task
    if (tasks.length === 0 && body.trim().length > 0) {
      tasks.push({
        number: `${phaseNum}.1`,
        title: phaseTitle,
        description: body.trim(),
        filePaths: extractFilePaths(body),
        acceptance: "",
      });
    }

    phases.push({
      number: phaseNum,
      title: phaseTitle,
      description: body.split("###")[0].trim(),
      tasks,
    });
  }

  const project = inferProject(raw);
  return { name, filePath, title, context, project, createdAt: "", modifiedAt: "", phases };
}

export function parsePlanFile(filePath: string): ParsedPlan {
  const raw = fs.readFileSync(filePath, "utf-8");
  const name = path.basename(filePath, ".md");
  const plan = parsePlanMarkdown(raw, name, filePath);
  const stat = fs.statSync(filePath);
  plan.createdAt = stat.birthtime.toISOString();
  plan.modifiedAt = stat.mtime.toISOString();
  return plan;
}

export function listPlans(): PlanSummary[] {
  if (!fs.existsSync(PLANS_DIR)) return [];
  const files = fs.readdirSync(PLANS_DIR).filter((f) => f.endsWith(".md"));
  return files.map((f) => {
    const filePath = path.join(PLANS_DIR, f);
    const parsed = parsePlanFile(filePath);
    const taskCount = parsed.phases.reduce(
      (sum, p) => sum + p.tasks.length,
      0
    );
    return {
      name: parsed.name,
      title: parsed.title,
      project: parsed.project,
      phaseCount: parsed.phases.length,
      taskCount,
      createdAt: parsed.createdAt,
      modifiedAt: parsed.modifiedAt,
    };
  });
}
