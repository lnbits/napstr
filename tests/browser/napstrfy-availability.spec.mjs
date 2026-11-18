import { test, expect } from '@playwright/test';
import { mockNative, serveAudio } from './helpers/native.mjs';

async function openStaleResult(page, mode) {
  await mockNative(page, { remote: true });
  await page.route('**/fixture.wav', serveAudio);
  await page.addInitScript((mode) => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    window.hostAvailable = mode === 'already-local';
    window.hostRevision = 1;
    window.__TAURI_INTERNALS__.invoke = async (cmd, args = {}) => {
      if (cmd === 'companion_status') return { ...await invoke(cmd, args), libraryRevision: window.hostRevision };
      if (cmd === 'remote_available') {
        window.calls.push({ cmd, args });
        if (mode === 'old-host') throw 'invalid Napstrfy request';
        return window.hostAvailable ? args.fileIds : [];
      }
      if (cmd === 'remote_download') {
        window.calls.push({ cmd, args });
        if (mode === 'race') window.hostAvailable = true;
        if (window.hostAvailable) throw 'this audio is already on this computer; play it locally';
        if (mode === 'failed') throw 'Seeder unavailable';
        return 'request-1';
      }
      if (cmd === 'remote_transfers') return [];
      return invoke(cmd, args);
    };
  }, mode);
  await page.goto('http://127.0.0.1:15174');
  await expect(page.locator('.track-row.remote')).toHaveCount(1);
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
}

for (const mode of ['already-local', 'race']) {
  test(`Napstrfy plays a newly downloaded song from a stale result: ${mode}`, async ({ page }) => {
    await openStaleResult(page, mode);
    await page.locator('.track-open').click();
    await expect(page.locator('.track-row.remote')).toHaveCount(0);
    await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
    await expect(page.locator('.error-banner')).toHaveCount(0);
    const calls = await page.evaluate(() => window.calls);
    expect(calls.filter(({ cmd }) => cmd === 'remote_download')).toHaveLength(mode === 'race' ? 1 : 0);
    expect(calls.filter(({ cmd }) => cmd === 'cache_remote_audio')).toHaveLength(1);
    expect(calls.find(({ cmd }) => cmd === 'cache_remote_audio').args.track.local).toBe(true);
  });
}

test('a real download error remains visible when the host does not have the file', async ({ page }) => {
  await openStaleResult(page, 'failed');
  await page.locator('.track-open').click();
  await expect(page.locator('.error-banner')).toContainText('Seeder unavailable');
  await expect(page.locator('.track-row.remote')).toHaveCount(1);
  expect(await page.evaluate(() => window.calls.some(({ cmd }) => cmd === 'cache_remote_audio'))).toBe(false);
});

test('transfer completion uses the file hash rather than searching the publisher filename', async ({ page }) => {
  await openStaleResult(page, 'pending');
  await page.locator('.track-open').click();
  await expect(page.locator('.notice-banner')).toContainText('over Tor');
  await page.evaluate(() => { window.hostAvailable = true; });
  await expect(page.locator('.track-row.remote')).toHaveCount(0, { timeout: 8000 });
  await expect(page.locator('.notice-banner')).toContainText('ready to play');
  expect(await page.evaluate(() => window.calls.some(({ cmd, args }) => cmd === 'remote_library' && args.query))).toBe(false);
  await page.locator('.track-open').click();
  await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
});

test('an open search updates when a download elsewhere changes the host library', async ({ page }) => {
  await openStaleResult(page, 'pending');
  const search = page.getByRole('textbox', { name: 'Search tracks', exact: true });
  await search.fill('little kim');
  await search.press('Enter');
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.track-row.remote')).toHaveCount(1);
  await page.evaluate(() => {
    window.hostAvailable = true;
    window.hostRevision = 2;
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect(page.locator('.track-row.remote')).toHaveCount(0);
  await expect(search).toHaveValue('little kim');
  expect(await page.evaluate(() => window.calls.some(({ cmd }) => cmd === 'remote_download'))).toBe(false);
});

test('older hosts without hash availability can still accept download requests', async ({ page }) => {
  await openStaleResult(page, 'old-host');
  await page.locator('.track-open').click();
  await expect.poll(() => page.evaluate(() => window.calls.filter(({ cmd }) => cmd === 'remote_download').length)).toBe(1);
  await expect(page.locator('.error-banner')).toHaveCount(0);
});

test('a delayed song availability check does not interrupt a podcast started meanwhile', async ({ page }) => {
  await openStaleResult(page, 'pending');
  await page.evaluate(() => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    let held = false;
    window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
      if (cmd === 'remote_available' && !held) {
        held = true;
        return new Promise((resolve) => { window.releaseAvailability = () => resolve(args.fileIds); });
      }
      return invoke(cmd, args);
    };
  });
  await page.locator('.track-open').click();
  await expect.poll(() => page.evaluate(() => typeof window.releaseAvailability)).toBe('function');
  await page.locator('.app-nav button').nth(1).click();
  await page.locator('.podcast-open').first().click();
  await page.locator('.episode-copy').click();
  await expect(page.locator('.now-copy strong')).toHaveText('Original episode');
  await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
  await page.evaluate(() => window.releaseAvailability());
  await page.locator('.app-nav button').first().click();
  await expect(page.locator('.track-row.remote')).toHaveCount(0);
  await expect(page.locator('.now-copy strong')).toHaveText('Original episode');
  expect(await page.evaluate(() => window.calls.some(({ cmd }) => cmd === 'cache_remote_audio'))).toBe(false);
});
