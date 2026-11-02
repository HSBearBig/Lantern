import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './e2e',
  use: { baseURL: 'http://127.0.0.1:4173', channel: process.env.PLAYWRIGHT_CHROMIUM === 'bundled' ? undefined : 'chrome' },
  webServer: [
    { command: 'cargo run --manifest-path ../../Cargo.toml -p lantern-signaling -- --origin http://127.0.0.1:4173', url: 'http://127.0.0.1:8787/healthz', reuseExistingServer: !process.env.CI, timeout: 120_000 },
    { command: 'npm run dev -- --host 127.0.0.1 --port 4173', url: 'http://127.0.0.1:4173', reuseExistingServer: !process.env.CI },
  ],
});
