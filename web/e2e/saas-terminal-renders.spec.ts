// SaaS terminal-renders e2e — pins the bug fixed in
// `terminal_ws.rs::handle_terminal_remote`. Before that fix the dashboard's
// `/terminal?agent=<id>` WebSocket only knew how to talk to in-process
// agents (`registry.agents`); SaaS agents live on the runner so the
// lookup missed and the server replied with `--- terminal detached ---`.
//
// Boot is owned by `tests/e2e/run-playwright-saas.sh` (per CLAUDE.md /
// ADR 0005 — host-process tests are forbidden). The runner script:
//   1. Brings up `deploy/docker-compose.e2e.yml` saas profile with the
//      `e2e-stub.yml` overlay (mounts `tests/e2e/stub-claude-tui.sh` as
//      `/usr/local/bin/claude` in the runner container).
//   2. Signs up a user via `/api/auth/signup` and mints a runner token
//      via `/api/runners/tokens`; brings the runner up with the token.
//   3. `docker cp`s `saas-terminal-renders.plan.yaml` into the server's
//      `/data/plans/`; pre-creates `/home/branchwork/<plan>` in the
//      runner container so the supervisor's chdir doesn't ENOENT.
//   4. Exports `BRANCHWORK_E2E_BASE_URL`, `BRANCHWORK_E2E_EMAIL`,
//      `BRANCHWORK_E2E_PASSWORD` and runs Playwright with the
//      `playwright.containerized.config.ts` config.
//
// The TUI stub re-paints `BRANCHWORK-TUI-READY` (clear-screen + cursor
// home + sentinel + CRLF) every 500 ms. The two cases below pin:
//   • case (a): output is bridged from runner → server → dashboard. The
//     fix in `handle_terminal_remote` subscribes to `broadcast_tx`,
//     filters `agent_output` events for our agent, base64-decodes, and
//     forwards to the WS. Without the fix the WS receives only the
//     "terminal detached" banner and the assertion times out.
//   • case (b): xterm's `resync()` (Task 4.2) clears the buffer on
//     resize. The stub's 500 ms repaint cycle should refill it; the
//     assertion gives 6 s to land at least one repaint after the resize.

import { test, expect, type Page, type APIRequestContext } from "@playwright/test";

const PLAN_NAME = "saas-terminal-renders";
const TASK_NUMBER = "0.1";
const SENTINEL = "BRANCHWORK-TUI-READY";

const EMAIL =
  process.env.BRANCHWORK_E2E_EMAIL ??
  (() => {
    throw new Error("BRANCHWORK_E2E_EMAIL is required");
  })();
const PASSWORD =
  process.env.BRANCHWORK_E2E_PASSWORD ??
  (() => {
    throw new Error("BRANCHWORK_E2E_PASSWORD is required");
  })();

test.describe.configure({ mode: "serial" });

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByPlaceholder("you@example.com").fill(EMAIL);
  await page.getByPlaceholder("********").fill(PASSWORD);
  await page.getByRole("button", { name: /^Sign in$/ }).click();
  await expect(page.getByRole("button", { name: /^Sign out$/ })).toBeVisible({
    timeout: 30_000,
  });
}

async function clickStartIfPresent(page: Page) {
  await page.goto(`/plans/${PLAN_NAME}`, { waitUntil: "networkidle" });
  const startButton = page.getByRole("button", { name: /^Start$/ });
  // Fast-path skip if a previous test in the same file already started
  // the task and the agent is still running.
  if (await startButton.isVisible({ timeout: 2_000 }).catch(() => false)) {
    await startButton.click();
  }
}

/// Resolve the running agent's id by polling the API. The agents list is
/// a bare array (not `{agents: [...]}`); see runner_ws.rs:680 — it returns
/// `Json(rows)` directly. Filters by `plan_name` + `task_id` so we don't
/// pick up a stale agent from a previous spec run.
async function waitForRunningAgent(
  request: APIRequestContext,
  timeoutMs = 30_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  while (Date.now() < deadline) {
    const res = await request.get("/api/agents");
    if (res.ok()) {
      const rows = (await res.json()) as Array<{
        id: string;
        status: string;
        plan_name: string | null;
        task_id: string | null;
      }>;
      const match = rows.find(
        (r) =>
          r.plan_name === PLAN_NAME &&
          r.task_id === TASK_NUMBER &&
          (r.status === "running" || r.status === "starting"),
      );
      if (match) return match.id;
      last = JSON.stringify(rows.map((r) => ({ id: r.id, status: r.status })));
    } else {
      last = `HTTP ${res.status()}`;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`No running agent for ${PLAN_NAME}/${TASK_NUMBER} within ${timeoutMs}ms (last=${last})`);
}

test("agent terminal renders bytes within 5 s of start (SaaS bridge)", async ({ page, request }) => {
  test.setTimeout(120_000);
  await signIn(page);
  await clickStartIfPresent(page);

  // Resolve the running agent's id via API rather than UI selectors —
  // task-card click + right-rail navigation has multiple paths in the
  // dashboard depending on viewport, and this test only cares about
  // `/agents/<id>` rendering the PtyTerminal.
  const agentId = await waitForRunningAgent(request);
  await page.goto(`/agents/${agentId}`, { waitUntil: "networkidle" });

  // PtyTerminal is lazy-loaded (AgentPanel.tsx:15), so xterm's DOM
  // appears a tick after the agent route mounts. `.xterm-screen` is the
  // outer container xterm injects into the host div on `term.open(...)`.
  await expect(page.locator(".xterm-screen").first()).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.locator(".xterm-screen").first()).toContainText(SENTINEL, {
    timeout: 5_000,
  });
});

test("sentinel survives viewport resize (SaaS bridge + Task 4.2)", async ({ page, request }) => {
  test.setTimeout(120_000);
  await signIn(page);
  await clickStartIfPresent(page);

  const agentId = await waitForRunningAgent(request);
  await page.goto(`/agents/${agentId}`, { waitUntil: "networkidle" });

  await expect(page.locator(".xterm-screen").first()).toBeVisible({
    timeout: 15_000,
  });
  await expect(page.locator(".xterm-screen").first()).toContainText(SENTINEL, {
    timeout: 5_000,
  });

  // Shrink the viewport — fires xterm's ResizeObserver → resync(), which
  // calls `term.reset()` (drops the visible buffer), then sends Ctrl+L
  // and a resize JSON to the server. The stub repaints every 500 ms so
  // within 6 s we should see the sentinel back in xterm.
  await page.setViewportSize({ width: 800, height: 600 });
  await expect(page.locator(".xterm-screen").first()).toContainText(SENTINEL, {
    timeout: 6_000,
  });
});
