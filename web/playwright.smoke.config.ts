import { defineConfig, devices } from "@playwright/test";

// Smoke test config for the containerized SaaS stack — drives the new-plan
// form against a runner round-trip end-to-end. Bootstrapped by
// tests/smoke/run.sh, which writes BASE_URL / SMOKE_PROJECT / SMOKE_USER_*
// into the environment before invoking `pnpm --filter @branchwork/web
// test:smoke`.
//
// The spec exercises four scenarios from the plan brief:
//   1. Folder suggestions list runner-side dirs
//   2. no_runner_connected banner when the runner is stopped
//   3. createFolder=true round-trip lands on the runner FS
//   4. runner_unavailable when the runner is killed mid-flight
//
// We do not run any vitest tests from here — vitest is configured to read
// `src/**/*.{test,spec}.{ts,tsx}` only, so this config is a separate axis.
const baseURL = process.env.BASE_URL ?? "http://localhost:3199";
const artifactsDir = process.env.SMOKE_ARTIFACTS_DIR ?? "../tests/smoke-artifacts/_default";

export default defineConfig({
  testDir: "./e2e",
  testMatch: /runner-banners\.spec\.ts$/,
  fullyParallel: false, // The four scenarios share a single runner container.
  workers: 1,
  retries: 0,
  reporter: [["list"], ["html", { outputFolder: `${artifactsDir}/html-report`, open: "never" }]],
  outputDir: `${artifactsDir}/test-results`,
  timeout: 60_000,
  expect: { timeout: 10_000 },
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
