// README golden path — SaaS mode. Mirrors `golden-path.spec.ts` step by
// step but registers a stub runner first so `org_has_runner` flips and
// folder/start-agent/merge calls dispatch over the runner WebSocket.
//
// The runner is the actual `branchwork-runner` binary, spawned by the
// support helper with the same stub claude on PATH that the standalone
// spec uses. Identical UI assertions to prove the dashboard behaves the
// same in both modes — that is the brief's "assert identical UI
// behaviour" acceptance.

import { test, expect, type Page } from "@playwright/test";
import { promises as fs } from "node:fs";
import * as path from "node:path";

import { readRuntime, type RuntimeInfo } from "./support/test-server";
import { StubRunner, waitForRunnerOnline } from "./support/stub-runner";

const PLAN_NAME = "saas-golden-path";
const PROJECT_NAME = "saas-golden-path-project";
const TASK_NUMBER = "0.1";

let runtime: RuntimeInfo;
let runner: StubRunner | null = null;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  runtime = await readRuntime();
});

test.afterEach(async () => {
  if (runner) {
    await runner.stop();
    runner = null;
  }
});

test("README golden path runs end-to-end against a SaaS runner", async ({ page }) => {
  test.setTimeout(180_000);

  const email = uniqueEmail("saas");
  const password = "e2e-password-1234";

  // 1. Sign up. Same form as the standalone spec.
  await page.goto("/");
  await page.getByRole("button", { name: /Need an account\? Sign up/i }).click();
  await page.getByPlaceholder("you@example.com").fill(email);
  await page.getByPlaceholder("********").fill(password);
  await page.getByRole("button", { name: /^Sign up$/ }).click();
  await expect(page.getByRole("button", { name: /^Sign out$/ })).toBeVisible({
    timeout: 30_000,
  });

  // 1a. Mint a runner token and bring up the stub runner. The token API
  //     uses the session cookie set by signup. We can read the cookie
  //     out of the browser context.
  const cookies = await page.context().cookies();
  const cookieHeader = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  const tokenRes = await fetch(`${runtime.apiBase}/api/runners/tokens`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Cookie: cookieHeader },
    body: JSON.stringify({ runner_name: "e2e-stub-runner" }),
  });
  const tokenText = await tokenRes.text();
  expect(tokenRes.status, tokenText).toBe(201);
  const tokenBody = JSON.parse(tokenText) as { token: string };

  runner = new StubRunner({
    runtime,
    token: tokenBody.token,
    runnerId: `e2e-runner-${Date.now()}`,
    cwd: runtime.tempDir,
  });
  await runner.start();
  await waitForRunnerOnline(runtime.apiBase, cookieHeader);

  // 2 + 3. Create the plan. Folder lookup + creation is dispatched over
  //        the runner WS, but the form behaves identically to the
  //        standalone case — same banner, same confirm prompt.
  await page.goto("/new-plan");
  await expect(page.getByRole("heading", { name: /^New Plan$/ })).toBeVisible();
  await page.getByPlaceholder(/~\/my-project/).fill(`~/${PROJECT_NAME}`);
  await page
    .getByPlaceholder(/Describe the feature|describe anything/i)
    .first()
    .fill("SaaS-mode hello world for the e2e golden path.");
  await page.getByRole("button", { name: /^Create Plan$/ }).click();

  await expect(page.getByText(/Directory does not exist/i)).toBeVisible({
    timeout: 15_000,
  });
  await page.getByRole("button", { name: /Create folder and continue/i }).click();
  await expect(page.getByRole("heading", { name: /^New Plan$/ })).toBeHidden({
    timeout: 30_000,
  });

  // 4. Wait for the planner agent to write the YAML. Plans dir lives
  //    under HOME=runtime.tempDir for the server; the runner-spawned
  //    daemon inherits the same HOME via PATH/env in the stub launcher.
  await waitForPlanFile(runtime.tempDir, PLAN_NAME);

  // 5. Open the plan and click Start.
  await page.goto(`/plans/${PLAN_NAME}`, { waitUntil: "networkidle" });
  const startButton = page.getByRole("button", { name: /^Start$/ });
  await expect(startButton).toBeVisible({ timeout: 30_000 });
  await expect(startButton).toBeEnabled({ timeout: 15_000 });
  await startButton.click();

  // 6. Stub claude commits, PUTs status=completed, fires Stop hook —
  //    same flow as standalone, just routed through the runner.
  await expectTaskStatus(page, PLAN_NAME, TASK_NUMBER, "completed", 60_000);

  // 7. Merge.
  const mergeButton = page.getByRole("button", { name: /^Merge$/ });
  await expect(mergeButton.first()).toBeVisible({ timeout: 30_000 });
  await mergeButton.first().click();

  // 8. Verify completion + merge cleanup.
  await expectTaskStatus(page, PLAN_NAME, TASK_NUMBER, "completed", 30_000);
  await expect(mergeButton.first()).toBeHidden({ timeout: 15_000 });
});

function uniqueEmail(prefix: string): string {
  const ts = Date.now();
  const rand = Math.floor(Math.random() * 1e6);
  return `${prefix}-${ts}-${rand}@example.test`;
}

async function waitForPlanFile(homeDir: string, planName: string, timeoutMs = 30_000): Promise<void> {
  const filePath = path.join(homeDir, ".claude", "plans", `${planName}.yaml`);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      await fs.access(filePath);
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 250));
    }
  }
  throw new Error(`plan file ${filePath} did not appear within ${timeoutMs}ms`);
}

async function expectTaskStatus(
  page: Page,
  planName: string,
  taskNumber: string,
  expected: string,
  timeoutMs: number
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const res = await page.request.get(`/api/plans/${planName}`);
    if (res.ok()) {
      const body = (await res.json()) as {
        phases?: Array<{ tasks?: Array<{ number?: string; status?: string }> }>;
      };
      const tasks = body.phases?.flatMap((p) => p.tasks ?? []) ?? [];
      const task = tasks.find((t) => t.number === taskNumber);
      last = task?.status ?? "(missing)";
      if (last === expected) return;
    } else {
      last = `${res.status()}`;
    }
    await page.waitForTimeout(500);
  }
  throw new Error(
    `task ${planName}/${taskNumber} did not reach status=${expected} within ${timeoutMs}ms (last=${last})`
  );
}
