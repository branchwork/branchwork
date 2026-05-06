// Test fixture for the e2e golden-path spec — spawns a fresh
// `branchwork-server` against a tempdir + stub claude, then a Vite dev
// server proxied at it. Used by playwright.config.ts as the `webServer`
// command.
//
// Why a TS bootstrap rather than two separate `webServer` entries:
// Playwright spawns each `webServer` command in its own shell with a
// static `env`, so we can't dynamically thread the rust server's port
// into Vite's proxy from the config alone. The bootstrap owns the
// linkage — picks a free rust port, spawns the rust server with a
// stubbin PATH, then spawns Vite with `BRANCHWORK_BACKEND_PORT` so
// `vite.config.ts` proxies to the right backend. On SIGTERM/SIGINT the
// bootstrap kills both children and removes the tempdir.

import { spawn, type ChildProcess } from "node:child_process";
import { createServer } from "node:net";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { installStubClaude } from "./stub-claude.ts";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "../../..");
const SERVER_BIN = path.join(REPO_ROOT, "server-rs/target/release/branchwork-server");
const RUNNER_BIN = path.join(REPO_ROOT, "server-rs/target/release/branchwork-runner");
const RUNTIME_FILE = path.join(REPO_ROOT, "web/.playwright-runtime.json");
const VITE_PORT_DEFAULT = 5174;

export interface RuntimeInfo {
  rustPort: number;
  vitePort: number;
  baseUrl: string; // Vite dev URL — playwright `baseURL`.
  apiBase: string; // Direct rust URL for tests that bypass the UI.
  tempDir: string; // Server `--claude-dir` parent (HOME=tempDir).
  stubBinDir: string;
  serverBin: string;
  runnerBin: string;
  hookUrl: string;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.unref();
    s.on("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const addr = s.address();
      if (addr && typeof addr === "object") {
        const port = addr.port;
        s.close(() => resolve(port));
      } else {
        s.close();
        reject(new Error("could not allocate free port"));
      }
    });
  });
}

async function waitFor(url: string, label: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastErr = "";
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url, { method: "GET" });
      if (res.ok) return;
      lastErr = `${res.status}`;
    } catch (e) {
      lastErr = (e as Error).message;
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timed out waiting for ${label} at ${url} (last=${lastErr})`);
}

interface Bootstrap {
  runtime: RuntimeInfo;
  rust: ChildProcess;
  vite: ChildProcess;
  cleanup: () => Promise<void>;
}

export async function bootstrap(): Promise<Bootstrap> {
  for (const bin of [SERVER_BIN, RUNNER_BIN]) {
    if (!(await fileExists(bin))) {
      throw new Error(
        `${bin} missing — run \`cargo build --release -p branchwork-server\` from server-rs/ first`
      );
    }
  }

  const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "branchwork-e2e-"));
  const claudeDir = path.join(tempDir, ".claude");
  const stubBinDir = path.join(tempDir, "stubbin");
  await fs.mkdir(claudeDir, { recursive: true });
  await fs.mkdir(path.join(claudeDir, "plans"), { recursive: true });
  await installStubClaude(stubBinDir);

  const rustPort = await freePort();
  const vitePort = Number(process.env.BRANCHWORK_E2E_VITE_PORT ?? VITE_PORT_DEFAULT);
  const baseUrl = `http://localhost:${vitePort}`;
  const apiBase = `http://127.0.0.1:${rustPort}`;
  const hookUrl = `${apiBase}/hooks`;

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    HOME: tempDir,
    USERPROFILE: tempDir,
    PATH: `${stubBinDir}${path.delimiter}${process.env.PATH ?? ""}`,
    BRANCHWORK_HOOK_URL: hookUrl,
    BRANCHWORK_STUB_PLANS_DIR: path.join(claudeDir, "plans"),
    // Bypasses ClaudeDriver::auth_status's interactive OAuth gate so the
    // dashboard renders the Start button enabled. The stub claude never
    // actually reads the var; it's purely the signal the server uses to
    // classify the driver as Ready.
    ANTHROPIC_API_KEY: process.env.ANTHROPIC_API_KEY ?? "e2e-stub-key",
  };

  const rust = spawn(
    SERVER_BIN,
    ["--port", String(rustPort), "--claude-dir", claudeDir],
    { env, stdio: ["ignore", "pipe", "pipe"] }
  );
  pipeOutput(rust, "[rust]");

  await waitFor(`${apiBase}/api/health`, "rust server");

  const vite = spawn(
    "pnpm",
    ["exec", "vite", "--port", String(vitePort), "--host", "127.0.0.1", "--strictPort"],
    {
      cwd: path.join(REPO_ROOT, "web"),
      env: {
        ...process.env,
        BRANCHWORK_BACKEND_PORT: String(rustPort),
      },
      stdio: ["ignore", "pipe", "pipe"],
    }
  );
  pipeOutput(vite, "[vite]");

  await waitFor(baseUrl, "vite dev server");

  const runtime: RuntimeInfo = {
    rustPort,
    vitePort,
    baseUrl,
    apiBase,
    tempDir,
    stubBinDir,
    serverBin: SERVER_BIN,
    runnerBin: RUNNER_BIN,
    hookUrl,
  };
  await fs.writeFile(RUNTIME_FILE, JSON.stringify(runtime, null, 2));

  const cleanup = async () => {
    for (const child of [rust, vite]) {
      try {
        if (!child.killed) child.kill("SIGTERM");
      } catch {
        // best-effort
      }
    }
    await Promise.allSettled([waitExit(rust, 5000), waitExit(vite, 5000)]);
    await fs.rm(RUNTIME_FILE, { force: true });
    await fs.rm(tempDir, { recursive: true, force: true });
  };

  return { runtime, rust, vite, cleanup };
}

async function fileExists(p: string): Promise<boolean> {
  try {
    await fs.access(p);
    return true;
  } catch {
    return false;
  }
}

function pipeOutput(child: ChildProcess, prefix: string) {
  const trace = process.env.BRANCHWORK_E2E_TRACE === "1";
  child.stdout?.on("data", (d) => {
    if (trace) process.stdout.write(`${prefix} ${d}`);
  });
  child.stderr?.on("data", (d) => {
    if (trace) process.stderr.write(`${prefix} ${d}`);
  });
}

function waitExit(child: ChildProcess, timeoutMs: number): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      try {
        if (!child.killed) child.kill("SIGKILL");
      } catch {
        // ignore
      }
      resolve();
    }, timeoutMs);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

/** Read the runtime info dropped by `bootstrap()`. Used inside test specs. */
export async function readRuntime(): Promise<RuntimeInfo> {
  const raw = await fs.readFile(RUNTIME_FILE, "utf-8");
  return JSON.parse(raw) as RuntimeInfo;
}

/** When invoked directly (not imported), bootstrap and stay alive until killed. */
async function runAsScript() {
  const b = await bootstrap();
  const onSignal = async () => {
    await b.cleanup();
    process.exit(0);
  };
  process.on("SIGTERM", onSignal);
  process.on("SIGINT", onSignal);
  // Match Playwright's webServer ready-cue: emit a recognisable line so
  // CI logs reveal the bootstrap completed even if the URL probe races.
  console.log(`[bootstrap] ready: rust=${b.runtime.apiBase} vite=${b.runtime.baseUrl}`);
  // Stay alive — Playwright kills us when the test run finishes.
  await new Promise(() => {});
}

const isDirectInvocation = (() => {
  try {
    return process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
  } catch {
    return false;
  }
})();

if (isDirectInvocation) {
  runAsScript().catch((e) => {
    console.error("[bootstrap] failed:", e);
    process.exit(1);
  });
}
