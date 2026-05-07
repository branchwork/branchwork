// Helper for the SaaS golden-path spec: drives the actual
// `branchwork-runner` binary against the test rust server with stub claude
// on PATH. The runner registers itself via `wss://<server>/ws/runner`,
// reports `online` at `/api/runners`, and routes folder/start-agent
// requests over the same WS connection — exactly what a real SaaS
// deployment does.
//
// Each instance is scoped to one test (per-org) so the `org_has_runner`
// gate flips when the runner registers and back when it disconnects. The
// runner's cwd is its own subdir under the bootstrap tempdir so the
// standalone spec's `~/golden-path-project` cannot collide with the
// SaaS spec's `~/saas-golden-path-project`.

import { spawn, type ChildProcess } from "node:child_process";
import { promises as fs } from "node:fs";
import * as path from "node:path";

import type { RuntimeInfo } from "./test-server";

interface StubRunnerOptions {
  runtime: RuntimeInfo;
  /** API token minted via POST /api/runners/tokens (caller fetches it). */
  token: string;
  /** Friendly id used in --runner-id and the runner's local DB. */
  runnerId: string;
  /** Directory the runner manages (parent for project subdirs). */
  cwd: string;
  /** Optional override for the runner's outbox DB. Defaults to a tempfile. */
  dbPath?: string;
}

export class StubRunner {
  private child: ChildProcess | null = null;
  private exited: Promise<void> | null = null;

  constructor(private readonly opts: StubRunnerOptions) {}

  async start(): Promise<void> {
    const { runtime, token, runnerId, cwd } = this.opts;
    await fs.mkdir(cwd, { recursive: true });
    const dbPath = this.opts.dbPath ?? path.join(cwd, ".runner.db");

    this.child = spawn(
      runtime.runnerBin,
      [
        "--saas-url",
        `ws://127.0.0.1:${runtime.rustPort}`,
        "--token",
        token,
        "--runner-id",
        runnerId,
        "--cwd",
        cwd,
        "--db-path",
        dbPath,
        "--server-bin",
        runtime.serverBin,
      ],
      {
        env: {
          ...process.env,
          HOME: runtime.tempDir,
          USERPROFILE: runtime.tempDir,
          PATH: `${runtime.stubBinDir}${path.delimiter}${process.env.PATH ?? ""}`,
          BRANCHWORK_HOOK_URL: runtime.hookUrl,
          BRANCHWORK_STUB_PLANS_DIR: path.join(runtime.tempDir, ".claude/plans"),
          // Same auth bypass as the standalone server — see test-server.ts.
          ANTHROPIC_API_KEY: process.env.ANTHROPIC_API_KEY ?? "e2e-stub-key",
        },
        stdio: ["ignore", "pipe", "pipe"],
      },
    );

    const trace = process.env.BRANCHWORK_E2E_TRACE === "1";
    this.child.stdout?.on("data", (d) => {
      if (trace) process.stdout.write(`[runner ${runnerId}] ${d}`);
    });
    this.child.stderr?.on("data", (d) => {
      if (trace) process.stderr.write(`[runner ${runnerId}] ${d}`);
    });

    this.exited = new Promise((resolve) => {
      this.child!.once("exit", () => resolve());
    });
  }

  async stop(): Promise<void> {
    if (!this.child) return;
    if (!this.child.killed) {
      try {
        this.child.kill("SIGTERM");
      } catch {
        // ignore
      }
    }
    await Promise.race([this.exited, new Promise<void>((resolve) => setTimeout(resolve, 5_000))]);
    if (this.child.exitCode === null && this.child.signalCode === null) {
      try {
        this.child.kill("SIGKILL");
      } catch {
        // ignore
      }
    }
    this.child = null;
  }
}

/**
 * Poll `/api/runners` until at least one runner reports `online`.
 * `cookie` is the Set-Cookie header from a prior signup/login response so
 * the call is authenticated to the right org.
 */
export async function waitForRunnerOnline(
  apiBase: string,
  cookie: string,
  timeoutMs = 20_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const res = await fetch(`${apiBase}/api/runners`, {
      headers: { Cookie: cookie },
    });
    if (res.ok) {
      const body = (await res.json()) as { runners?: Array<{ status?: string }> };
      last = body.runners?.[0]?.status ?? "";
      if (last === "online") return;
    } else {
      last = `${res.status}`;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`runner did not reach online within ${timeoutMs}ms (last=${last})`);
}
