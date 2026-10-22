import { defineConfig } from "@playwright/test";

/**
 * Frontend E2E. The Tauri IPC bridge is replaced with deterministic fixtures in
 * `tests/e2e/mock-tauri.ts`, so these tests never read or write real agent data.
 *
 * Native-window E2E (WebDriver) is a separate layer: see `docs/E2E_TESTING.md`.
 */
export default defineConfig({
  testDir: "./tests/e2e",
  timeout: 30_000,
  expect: { timeout: 8_000 },
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: "http://localhost:1420",
    // Use the installed Edge instead of downloading a Playwright browser.
    channel: "msedge",
    headless: true,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "pnpm dev",
    url: "http://localhost:1420",
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
