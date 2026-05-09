import { defineConfig, devices } from "@playwright/test";

// Playwright config for the containerized terminal-renders e2e
// (saas-terminal-renders.spec.ts). Unlike `playwright.config.ts` —
// which spawns rust-server + vite + stub-claude on the host via
// `e2e/support/test-server.ts` — this config has NO `webServer`. It
// expects the SaaS stack to be up via Docker Compose (the bash runner
// at `tests/e2e/run-playwright-saas.sh` owns that lifecycle, per
// CLAUDE.md / ADR 0005).
//
// `BRANCHWORK_E2E_BASE_URL` is the only required env var. The runner
// script sets it to the host port the compose stack publishes (3199 by
// default). The dashboard is served by `branchwork-server` itself in
// production mode (embedded web bundle), so there's no Vite layer.
//
// Only the new spec runs here (testMatch). The existing host-based
// suite (`golden-path.spec.ts`, `saas-golden-path.spec.ts`) keeps
// using `playwright.config.ts` and is invoked by `pnpm e2e`.

const baseURL =
  process.env.BRANCHWORK_E2E_BASE_URL ??
  (() => {
    throw new Error(
      "BRANCHWORK_E2E_BASE_URL is required — boot the compose stack via tests/e2e/run-playwright-saas.sh",
    );
  })();

export default defineConfig({
  testDir: "./e2e",
  testMatch: ["**/saas-terminal-renders.spec.ts"],
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  outputDir: "./.playwright-artifacts/containerized",
  timeout: 120_000,
  expect: { timeout: 15_000 },
  use: {
    baseURL,
    headless: true,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
