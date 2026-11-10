import { defineConfig, devices } from "@playwright/test";

// PW_CHANNEL=chrome uses the installed Google Chrome (no browser download);
// unset, Playwright's own Chromium (`npx playwright install chromium`).
const channel = process.env.PW_CHANNEL || undefined;

export default defineConfig({
  testDir: "./tests",
  globalSetup: "./global-setup.js",
  // One bridge and one simulated scanner for the whole run: tests go one at a time.
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 45_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? [["list"], ["github"]] : [["list"]],
  outputDir: "./test-results",
  use: {
    // Coverage needs Chromium (V8); the bridge is on localhost only.
    ...devices["Desktop Chrome"],
    channel,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [{ name: channel ?? "chromium", use: { channel } }],
});
