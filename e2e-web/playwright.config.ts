import { defineConfig, devices } from '@playwright/test';

// The UI runs in a plain Chromium against the fake IPC backend in `src/`, so no Rust
// build, WebView2 or msedgedriver is involved: this suite runs on any OS and in CI.
//
// A port of its own: 5173 belongs to `pnpm tauri dev` and to whatever else the machine
// happens to run, and `reuseExistingServer` would happily reuse that server.
// `E2E_WEB_PORT` lets parallel checkouts (worktrees) each run their own server.
const PORT = Number(process.env.E2E_WEB_PORT ?? 5174);
const BASE_URL = `http://localhost:${PORT}`;

export default defineConfig({
  testDir: './specs',
  // One worker: the tests are fast, and a single Vite server serves them all.
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI
    ? [['list'], ['html', { open: 'never' }]]
    : [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL: BASE_URL,
    viewport: { width: 1440, height: 900 },
    // Evidence of a failure (the trace viewer shows the whole run step by step).
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } }],
  webServer: {
    // Its own Vite instance (the UI has no devUrl dependency: `--port` overrides the config).
    command: `pnpm --dir ../ui dev --port ${PORT} --strictPort`,
    url: BASE_URL,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});
