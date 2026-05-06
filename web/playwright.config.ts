import { defineConfig, devices } from "@playwright/test";

// Playwright config for the README golden-path e2e (task 8.1). The
// bootstrap helper at e2e/support/test-server.ts owns the rust-server +
// vite + stub-claude lifecycle: this config just hands Playwright a
// `webServer` command that runs it.
//
// The smoke spec (e2e/runner-banners.spec.ts) lives in its own config —
// see `playwright.smoke.config.ts`. It is not run by `pnpm e2e`.
const VITE_PORT = Number(process.env.BRANCHWORK_E2E_VITE_PORT ?? 5174);
const baseURL = `http://localhost:${VITE_PORT}`;

export default defineConfig({
  testDir: "./e2e",
  testIgnore: ["**/runner-banners.spec.ts"],
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  outputDir: "./.playwright-artifacts/test-results",
  timeout: 180_000,
  expect: { timeout: 15_000 },
  webServer: {
    command: "pnpm exec tsx e2e/support/test-server.ts",
    url: baseURL,
    reuseExistingServer: false,
    stdout: "pipe",
    stderr: "pipe",
    timeout: 120_000,
    env: {
      BRANCHWORK_E2E_VITE_PORT: String(VITE_PORT),
    },
  },
  use: {
    baseURL,
    headless: true,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
