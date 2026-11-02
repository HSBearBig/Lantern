import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';

test('loads the controller diagnostics shell', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Lantern' })).toBeVisible();
  await expect(page.getByText('尚未連線')).toBeVisible();
  await expect(page.getByTestId('remote-surface')).toBeVisible();
  await expect(page.getByRole('button', { name: '送出文字' })).toBeDisabled();
});

test('retains nonextractable browser identity across reload', async ({ page }) => {
  await page.goto('/');
  const identity = () => page.evaluate(async () => {
    const modulePath = '/src/store.ts';
    const { loadIdentity } = await import(/* @vite-ignore */ modulePath);
    const identity = await loadIdentity();
    return { id: identity.clientId as string, extractable: identity.keyPair.privateKey.extractable as boolean };
  });
  const first = await identity();
  expect(first.id).toMatch(/^[a-f0-9]{64}$/);
  expect(first.extractable).toBe(false);
  await page.reload();
  expect(await identity()).toEqual(first);
});

test('uses real signaling through the Vite proxy and preserves invite through StrictMode', async ({ page }) => {
  const vector = JSON.parse(readFileSync(new URL('../../../crates/host/tests/fixtures/webcrypto-vector.json', import.meta.url), 'utf8'));
  const host = await page.request.get('http://127.0.0.1:8787/healthz');
  expect((await host.json()).protocol).toBe(1);
  const hostId = await crypto.subtle.digest('SHA-256', Buffer.from(vector.hostKey, 'base64url'));
  const fragment = new URLSearchParams({ host: Buffer.from(hostId).toString('hex'), key: vector.hostKey, secret: vector.secret, name: 'StrictMode test host' });
  await page.goto(`/#${fragment}`);
  await expect(page.getByText('正在配對 StrictMode test host')).toBeVisible();
  await expect(page.getByText('連線失敗 · offline')).toBeVisible();
  expect(new URL(page.url()).hash).toBe('');
});
