import { defineConfig } from '@playwright/test';

// Smoke e2e runs against the mock client served by `vite preview`.
// Browsers are not installed by default: run `npx playwright install chromium` first.
export default defineConfig({
  testDir: 'e2e',
  fullyParallel: false,
  workers: 1,
  reporter: 'list',
  use: { baseURL: 'http://127.0.0.1:4173' },
  webServer: {
    command: 'npm run build && npm run preview -- --host 127.0.0.1 --port 4173',
    url: 'http://127.0.0.1:4173',
    env: { VITE_API: 'mock' },
    reuseExistingServer: true,
    timeout: 120_000
  }
});
