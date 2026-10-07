import { defineConfig, devices } from '@playwright/test';

// The browser tests: one server for the run, started by serve.sh over a fresh record, and
// a hosted copy of the pages on a second origin.
// Chromium only, as the Board Agenda Tool's suite is; the pages use nothing that is
// Chromium's alone, and one engine keeps the run short.
const port = Number(process.env.PORT ?? 4310);
const hosted = Number(process.env.HOSTED_PORT ?? 4311);

export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : [['list']],
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    trace: 'on-first-retry',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: [
    {
      command: './serve.sh',
      url: `http://127.0.0.1:${port}/`,
      reuseExistingServer: false,
      timeout: 60_000,
      stdout: 'pipe',
      stderr: 'pipe',
      env: { PORT: String(port), HOSTED_PORT: String(hosted) },
    },
    {
      // a hosted copy of the pages on another origin, plain files and no amem
      command: 'node tests/support/static.mjs',
      url: `http://127.0.0.1:${hosted}/index.html`,
      reuseExistingServer: false,
      timeout: 30_000,
      env: { HOSTED_PORT: String(hosted) },
    },
  ],
});
