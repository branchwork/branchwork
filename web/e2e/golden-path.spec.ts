// README golden path — standalone mode. Drives the README's eight-step
// scenario through the real dashboard UI against a fresh rust server +
// vite dev server (spawned by playwright.config.ts → e2e/support/test-server.ts).
//
// Steps mirror the README:
//   1. Sign up via the LoginPage form.
//   2. Open the New Plan form, pick a folder, describe a feature.
//   3. Confirm folder creation; the planner agent (stub claude) writes the YAML.
//   4. Wait for the plan to appear in the sidebar.
//   5. Open the plan; click Start on task 0.1.
//   6. Wait for the agent to commit, PUT task=completed, fire Stop hook
//      (CI is NotConfigured because the project has no .github/workflows
//      → "mock CI green").
//   7. Click Merge into master.
//   8. Verify task 0.1 status shows completed.

import { test, expect, type Page } from "@playwright/test";
import { promises as fs } from "node:fs";
import * as path from "node:path";

import { readRuntime, type RuntimeInfo } from "./support/test-server";

const PLAN_NAME = "golden-path";
const PROJECT_NAME = "golden-path-project";
const TASK_NUMBER = "0.1";

let runtime: RuntimeInfo;

test.describe.configure({ mode: "serial" });

test.beforeAll(async () => {
  runtime = await readRuntime();
});

test("README golden path runs end-to-end via the dashboard UI", async ({ page }) => {
  test.setTimeout(180_000);

  const email = uniqueEmail("standalone");
  const password = "e2e-password-1234";

  // 1. Sign up via the form. LoginPage starts in `login` mode; click the
  //    toggle to switch to `signup`.
  await page.goto("/");
  await page.getByRole("button", { name: /Need an account\? Sign up/i }).click();
  await page.getByPlaceholder("you@example.com").fill(email);
  await page.getByPlaceholder("********").fill(password);
  await page.getByRole("button", { name: /^Sign up$/ }).click();

  // After signup the auth gate flips and the dashboard renders. Use the
  // sign-out button as the "logged in" proxy — the sidebar is wrapped in
  // an `aria-hidden` aside until the user opens it (a11y bug tracked in
  // ui-store; not in scope for 8.1) so role-based sidebar selectors
  // would race with that flag.
  await expect(page.getByRole("button", { name: /^Sign out$/ })).toBeVisible({
    timeout: 30_000,
  });

  // 2 + 3. Open the new-plan form by URL — the sidebar's "+ New Plan"
  //        link is functionally identical and avoids the aria-hidden
  //        complication above.
  await page.goto("/new-plan");
  await expect(page.getByRole("heading", { name: /^New Plan$/ })).toBeVisible();

  await page.getByPlaceholder(/~\/my-project/).fill(`~/${PROJECT_NAME}`);
  await page
    .getByPlaceholder(/Describe the feature|describe anything/i)
    .first()
    .fill("Add a hello world feature for the e2e golden path.");
  await page.getByRole("button", { name: /^Create Plan$/ }).click();

  await expect(page.getByText(/Directory does not exist/i)).toBeVisible({
    timeout: 15_000,
  });
  await page.getByRole("button", { name: /Create folder and continue/i }).click();

  // The form closes onSuccess → Playwright sees the heading disappear.
  await expect(page.getByRole("heading", { name: /^New Plan$/ })).toBeHidden({
    timeout: 30_000,
  });

  // The stub claude (planner mode) now writes <plans-dir>/golden-path.yaml.
  // Wait for the file to appear on disk so the rest of the test does not
  // race the file watcher.
  await waitForPlanFile(runtime.tempDir, PLAN_NAME);

  // 4. Plan appears in the sidebar. Navigate directly to PlanBoard. The
  //    plan title button has aria-label="Edit plan title" and inner text
  //    "Golden Path"; matching by text rather than role avoids the
  //    aria-label override that role+name would otherwise resolve to.
  await page.goto(`/plans/${PLAN_NAME}`, { waitUntil: "networkidle" });

  // 5. Click Start on the only task. There is exactly one TaskCard in
  //    the plan, so a top-level Start button match is unambiguous. The
  //    Start button only enables once the driver auth check resolves —
  //    test-server.ts seeds ANTHROPIC_API_KEY so claude reports `api_key`.
  const startButton = page.getByRole("button", { name: /^Start$/ });
  await expect(startButton).toBeVisible({ timeout: 30_000 });
  await expect(startButton).toBeEnabled({ timeout: 15_000 });
  await startButton.click();

  // 6. The stub claude commits, PUTs task=completed, then fires the Stop
  //    hook. We wait for the task's state to render as completed by
  //    polling /api/plans/<plan> directly (UI mirrors this with up to
  //    a 2s WS debounce).
  await expectTaskStatus(page, PLAN_NAME, TASK_NUMBER, "completed", 60_000);

  // 7. Merge the branch. The TaskCard's merge banner only renders while
  //    the agent row carries `branch=branchwork/<plan>/<task>`. Click
  //    `Merge`; the server clears the branch field and the banner
  //    disappears. There is no confirmation dialog on Merge (only
  //    Discard has one — TaskCard.tsx:619).
  const mergeButton = page.getByRole("button", { name: /^Merge$/ });
  await expect(mergeButton.first()).toBeVisible({ timeout: 30_000 });
  await mergeButton.first().click();

  // 8. After merge, task is still completed (the stub set it before
  //    posting Stop) and the merge-banner disappears once branch=NULL.
  await expectTaskStatus(page, PLAN_NAME, TASK_NUMBER, "completed", 30_000);
  await expect(mergeButton.first()).toBeHidden({ timeout: 15_000 });
});

function uniqueEmail(prefix: string): string {
  const ts = Date.now();
  const rand = Math.floor(Math.random() * 1e6);
  return `${prefix}-${ts}-${rand}@example.test`;
}

async function waitForPlanFile(
  homeDir: string,
  planName: string,
  timeoutMs = 30_000,
): Promise<void> {
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
  timeoutMs: number,
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
    `task ${planName}/${taskNumber} did not reach status=${expected} within ${timeoutMs}ms (last=${last})`,
  );
}
