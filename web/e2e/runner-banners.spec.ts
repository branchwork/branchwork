// Smoke test for the SaaS new-plan UX — drives the four scenarios from the
// task 5.7 brief against a fully containerized stack (server + runner) and
// captures screenshots under tests/smoke-artifacts/<run-date>/ so a human
// reviewer can spot UX rough edges (banner placement, copy, dropdown
// loading state).
//
// Bootstrap: tests/smoke/run.sh stands up the SaaS docker-compose project
// with a unique name, signs up a user, mints a runner token, waits for
// `online`, and then invokes `pnpm --filter @branchwork/web test:smoke`
// with the following env populated:
//
//   BASE_URL              http://localhost:<random-port>
//   SMOKE_PROJECT         compose project name (unique per run)
//   SMOKE_USER_EMAIL      e2e-…@example.test
//   SMOKE_USER_PASSWORD   the password used during signup
//   SMOKE_ARTIFACTS_DIR   absolute path to tests/smoke-artifacts/<run-date>/
//   SMOKE_COMPOSE_FILE    absolute path to deploy/docker-compose.e2e.yml
//
// The spec MUST NOT scan or terminate host processes by pattern. Every
// "stop the runner / kill the runner" call goes through
// `docker compose -p $SMOKE_PROJECT …` so it is intrinsically scoped to
// the test's own containers — see ADR 0005 for the 2026-05-05 incident
// that motivated this rule.

import { test, expect, type Page } from "@playwright/test";
import { exec as execCb } from "node:child_process";
import { promisify } from "node:util";
import { randomBytes } from "node:crypto";

const exec = promisify(execCb);

const RUNNER_HOME = "/home/runneruser";

// Defer env lookups until tests actually run — this lets `playwright
// test --list` and unit tooling parse the file without the smoke
// harness having populated the environment.
function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) throw new Error(`smoke: ${name} must be set by tests/smoke/run.sh`);
  return v;
}

const PROJECT = (): string => requireEnv("SMOKE_PROJECT");
const COMPOSE_FILE = (): string => requireEnv("SMOKE_COMPOSE_FILE");
const USER_EMAIL = (): string => requireEnv("SMOKE_USER_EMAIL");
const USER_PASSWORD = (): string => requireEnv("SMOKE_USER_PASSWORD");
const ARTIFACTS = (): string => requireEnv("SMOKE_ARTIFACTS_DIR");

// Compose helpers — every container action goes through `-p $PROJECT` so
// teardown stays scoped. Do NOT replace these with host-process scans;
// see ADR 0005 for the rule and CLAUDE.md for the allowed fallbacks.
async function compose(args: string): Promise<string> {
  const cmd = `docker compose -p ${PROJECT()} -f ${COMPOSE_FILE()} ${args}`;
  const { stdout } = await exec(cmd, { timeout: 30_000 });
  return stdout;
}

async function runnerExec(shell: string): Promise<{ stdout: string; code: number }> {
  // Returns the exit code without throwing so callers can branch on
  // `test -d` failure (rc=1 means missing, rc=0 means exists).
  try {
    const { stdout } = await exec(
      `docker compose -p ${PROJECT()} -f ${COMPOSE_FILE()} exec -T branchwork-runner sh -c ${JSON.stringify(shell)}`,
      { timeout: 15_000 },
    );
    return { stdout, code: 0 };
  } catch (e) {
    const err = e as { stdout?: string; code?: number };
    return { stdout: err.stdout ?? "", code: err.code ?? 1 };
  }
}

async function waitForRunnerStatus(
  page: Page,
  want: "online" | "offline",
  timeoutMs = 20_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const res = await page.request.get("/api/runners");
    if (res.ok()) {
      const body = (await res.json()) as { runners?: Array<{ status?: string }> };
      last = body.runners?.[0]?.status ?? "";
      if (last === want) return;
    }
    await page.waitForTimeout(500);
  }
  throw new Error(`runner did not reach status=${want} within ${timeoutMs}ms (last=${last})`);
}

async function login(page: Page): Promise<void> {
  // Hit the API directly — the cookie is automatically attached to
  // subsequent navigations because the request is made through the
  // browser context's request fixture. This intentionally skips the
  // form to keep auth out of the runner-banner assertions; LoginPage is
  // covered by other tests.
  const res = await page.request.post("/api/auth/login", {
    data: { email: USER_EMAIL(), password: USER_PASSWORD() },
  });
  expect(res.status(), `login failed: ${res.status()} ${await res.text()}`).toBe(200);
}

async function openNewPlanForm(page: Page): Promise<void> {
  await page.goto("/");
  await page.getByRole("button", { name: /\+\s*New Plan/i }).click();
  await expect(page.getByRole("heading", { name: /New Plan/ })).toBeVisible();
}

async function shot(page: Page, name: string): Promise<void> {
  await page.screenshot({ path: `${ARTIFACTS()}/${name}.png`, fullPage: true });
}

// ── Tests ─────────────────────────────────────────────────────────────

test.describe.configure({ mode: "serial" }); // shared runner state

test.beforeAll(async () => {
  // Make sure runner is online before the suite starts. The bootstrap
  // script already waits for `online`, but earlier scenarios may have
  // left the runner stopped if a previous run was --keep'd.
  await compose("--profile saas start branchwork-runner").catch(() => {});
});

test.afterAll(async () => {
  // Restore runner to running state so subsequent local invocations or
  // human debugging sessions inherit a clean stack.
  await compose("--profile saas start branchwork-runner").catch(() => {});
});

test("1. folder suggestions list runner-side directories", async ({ page }) => {
  // Seed disjoint dirs on the runner FS — the dropdown must show these
  // and only these (server-side $HOME is /home/branchwork on the
  // server image; we verify the dropdown does not pick up server dirs
  // in tests/e2e/test_saas_list_folders.sh, so we focus on UX here).
  const sentinel = `smoke-folder-${randomBytes(3).toString("hex")}`;
  await runnerExec(`mkdir -p ${RUNNER_HOME}/${sentinel}`);

  await login(page);
  await openNewPlanForm(page);

  // Click into the folder input to surface the suggestions panel.
  const folderInput = page.getByPlaceholder(/~\/my-project/);
  await folderInput.click();

  // The dropdown is rendered as a stack of buttons with the dir name
  // (indigo span) and absolute path (gray span). Wait for our seeded
  // sentinel to appear.
  await expect(page.getByRole("button", { name: new RegExp(sentinel) })).toBeVisible();
  await shot(page, "01-folder-suggestions");
});

test("2. no_runner banner when runner is stopped", async ({ page }) => {
  await login(page);

  await compose("stop -t 1 branchwork-runner");
  await waitForRunnerStatus(page, "offline");

  // Open the form AFTER the disconnect so the initial GET /api/folders
  // returns 503 no_runner_connected and the amber banner renders.
  await openNewPlanForm(page);

  await expect(page.getByText(/No runner connected/i)).toBeVisible();
  // Dropdown must be empty / hidden — the suggestions list only renders
  // when `folders.length > 0 && !runnerStatus`.
  const folderInput = page.getByPlaceholder(/~\/my-project/);
  await folderInput.click();
  await expect(page.getByText(/^smoke-folder-/)).toHaveCount(0);
  await shot(page, "02-no-runner-banner");

  // Bring runner back and wait for `online` so subsequent tests inherit
  // a working stack.
  await compose("--profile saas start branchwork-runner");
  await waitForRunnerStatus(page, "online");
});

test("3. createFolder=true round-trips and lands on the runner FS", async ({ page }) => {
  const newDir = `smoke-create-${Date.now()}`;
  const newPath = `${RUNNER_HOME}/${newDir}`;

  // Confirm the dir does NOT exist on the runner before the test.
  const before = await runnerExec(`test -d ${newPath}`);
  expect(before.code, `precondition: ${newPath} should not exist`).not.toBe(0);

  await login(page);
  await openNewPlanForm(page);

  await page.getByPlaceholder(/~\/my-project/).fill(`~/${newDir}`);
  await page.getByPlaceholder(/Describe the feature/).fill("smoke test — folder creation flow");
  await page.getByRole("button", { name: /^Create Plan$/ }).click();

  // First submit returns 400 folder_not_found → confirm prompt shows.
  await expect(page.getByText(/Directory does not exist/i)).toBeVisible();
  await shot(page, "03a-create-folder-prompt");

  // Click the confirm button — second submit goes through with
  // createFolder=true and expects 200.
  await page.getByRole("button", { name: /Create folder and continue/i }).click();

  // The form closes onClose() → we end up back on the project dashboard.
  // Wait for the heading to disappear as the proxy for "submitted".
  await expect(page.getByRole("heading", { name: /^New Plan$/ })).toBeHidden({ timeout: 30_000 });

  // Verify the directory now exists on the runner FS.
  const after = await runnerExec(`test -d ${newPath}`);
  expect(after.code, `postcondition: ${newPath} should exist on runner after create`).toBe(0);
  await shot(page, "03b-after-create");

  // Cleanup so the runner volume stays roughly empty for future runs.
  await runnerExec(`rm -rf ${newPath}`);
});

test("4. runner_unavailable surfaces when runner is killed mid-flight", async ({ page }) => {
  await login(page);
  await waitForRunnerStatus(page, "online");

  await openNewPlanForm(page);
  await page
    .getByPlaceholder(/~\/my-project/)
    .fill(`~/smoke-killmid-${randomBytes(3).toString("hex")}`);
  await page
    .getByPlaceholder(/Describe the feature/)
    .fill("smoke test — runner_unavailable on mid-flight kill");

  // Pause the runner via the cgroups freezer — TCP stays alive, the
  // userspace runner process is SIGSTOPped, no responses come back.
  // This is the same trick the bash test (test_saas_no_runner.sh)
  // uses to make the in-flight window deterministic.
  await compose("pause branchwork-runner");

  // Submit; this fires POST /api/plans/create which dispatches a
  // ListFolders WS message to the (paused) runner.
  const responsePromise = page.waitForResponse(
    (r) => r.url().endsWith("/api/plans/create") && (r.status() === 504 || r.status() === 503),
    { timeout: 30_000 },
  );
  await page.getByRole("button", { name: /^Create Plan$/ }).click();

  // Give the request 200ms to land at the server, register in `pending`,
  // and ship the WireMessage over WS — comfortably inside the 8s helper
  // timeout, so the cleanup branch (drop pending senders on disconnect)
  // is the only code path that can resolve the request.
  await page.waitForTimeout(200);

  // SIGKILL the paused runner. TCP closes → server WS read loop sees
  // Closed → cleanup drains `pending` → our awaiting receiver wakes
  // with RunnerDisconnected → 504 runner_unavailable.
  await compose("kill -s SIGKILL branchwork-runner");

  const response = await responsePromise;
  // Either 504 (mid-flight kill caught the in-flight request) or 503
  // (the request landed AFTER the kill and the runner was already
  // marked offline). Both are valid for the "kill mid-flight" UX —
  // the form must surface a runner banner either way.
  expect([503, 504]).toContain(response.status());

  // The form's UI surfaces 504 as the red error message and 503 as the
  // amber RunnerBanner. Match either copy so the test is robust to the
  // race outcome.
  const status = response.status();
  if (status === 504) {
    await expect(page.getByText(/Runner did not respond in time/i)).toBeVisible({ timeout: 5_000 });
  } else {
    await expect(page.getByText(/No runner connected/i)).toBeVisible({ timeout: 5_000 });
  }
  await shot(page, "04-runner-unavailable");

  // Restore: bring the runner back so afterAll (and any human poking
  // at the stack with --keep) inherits a working state.
  await compose("--profile saas start branchwork-runner").catch(() => {});
  await waitForRunnerStatus(page, "online", 30_000);
});
