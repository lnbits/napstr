import { test, expect } from '@playwright/test';
import { mockNative } from './helpers/native.mjs';

test('Napstr compact player keeps only playback controls visible and restores the full window', async ({ page }) => {
  await mockNative(page, { app: 'napstr', saved: 'en' });
  await page.goto('http://127.0.0.1:15173');
  await expect(page.locator('.search-button')).toBeEnabled();

  await page.locator('.compact-toggle').click();
  await expect(page.locator('.app-window')).toHaveClass(/compact/);
  await expect(page.locator('.player-bar')).toBeVisible();
  await expect(page.locator('.resize-e')).toBeVisible();
  await expect(page.locator('.resize-w')).toBeVisible();
  await expect(page.locator('.resize-se')).toBeVisible();
  await expect(page.locator('.resize-n')).toBeHidden();
  await expect(page.locator('.resize-s')).toBeHidden();
  await expect(page.locator('.toolbar')).toBeHidden();
  await expect(page.locator('.network-strip')).toBeHidden();
  await expect(page.locator('.workspace')).toBeHidden();
  await expect(page.locator('.transfer-dock')).toBeHidden();
  await expect(page.locator('.statusbar')).toBeHidden();
  await expect.poll(() => page.evaluate(() => window.calls.some(({ cmd, args }) => cmd === 'set_compact_mode' && args.compact === true))).toBe(true);

  for (const width of [1180, 850, 650, 560]) {
    await page.setViewportSize({ width, height: 84 });
    const playerFits = await page.locator('.player-bar').evaluate((node) =>
      node.scrollWidth <= node.clientWidth + 2 && node.getBoundingClientRect().right <= innerWidth + 1
    );
    expect(playerFits, `compact player fits at ${width}px`).toBe(true);
  }

  await page.locator('.compact-toggle').click();
  await expect(page.locator('.app-window')).not.toHaveClass(/compact/);
  await expect(page.locator('.toolbar')).toBeVisible();
  await expect.poll(() => page.evaluate(() => window.calls.some(({ cmd, args }) => cmd === 'set_compact_mode' && args.compact === false))).toBe(true);
});

for (const language of ['en', 'fr', 'ar']) {
  test(`Napstr layout: compact panels and transfers remain usable in ${language}`, async ({ page }) => {
    test.setTimeout(90_000);
    await mockNative(page, { app: 'napstr', saved: language });
    await page.addInitScript(() => {
      const invoke = window.__TAURI_INTERNALS__.invoke;
      const name = 'A very long track filename with several words and a catalogue identifier ' + 'a'.repeat(64) + '.wav';
      const transfers = [
        { id: 1, fileId: 'a'.repeat(64), filename: name, size: 1234567, progress: 100, status: 'Verified · Complete', speed: '', destination: '/music/track.wav' },
        { id: 2, fileId: 'b'.repeat(64), filename: name, size: 1234567, progress: 0, status: 'Requesting Tor seeders', speed: 'Connecting…', destination: '' },
        { id: 3, fileId: 'c'.repeat(64), filename: name, size: 1234567, progress: 0, status: 'Queued', speed: 'Queued', destination: '' }
      ];
      window.__TAURI_INTERNALS__.invoke = async (cmd, args) => {
        if (cmd === 'get_transfers') return transfers;
        if (cmd === 'get_snapshot') {
          const snapshot = await invoke(cmd, args);
          const files = Array.from({ length: 105 }, (_, index) => ({ ...snapshot.files[0], fileId: index ? index.toString(16).padStart(64, '0') : 'a'.repeat(64), filename: name, title: name }));
          return { ...snapshot, files, transfers, settings: { ...snapshot.settings, displayName: 'A listener with a very long display name' } };
        }
        if (cmd === 'search_catalog') return (await window.__TAURI_INTERNALS__.invoke('get_snapshot')).files;
        return invoke(cmd, args);
      };
    });
    await page.goto('http://127.0.0.1:15173');
    await expect(page.locator('.search-button')).toBeEnabled();
    const views = ['Search', 'Downloads', 'Shared', 'Profile', 'Settings', 'Trollbox', 'Napstrfy'];
    for (const [width, height] of [[1180, 810], [851, 600], [850, 600], [651, 600], [650, 600], [560, 400]]) {
      await page.setViewportSize({ width, height });
      for (const [index, view] of views.entries()) {
        await page.locator('.tool-button').nth(index).click();
        await expect(page.locator('.mini-row')).toHaveCount(3);
        const misplaced = await page.locator('.mini-row').evaluateAll((rows) => rows.flatMap((row, index) => {
          const box = row.getBoundingClientRect();
          return [...row.children].filter((child) => {
            const rect = child.getBoundingClientRect();
            return rect.width && rect.height && (rect.top < box.top - 1 || rect.bottom > box.bottom + 1 || rect.left < box.left - 1 || rect.right > box.right + 1);
          }).map((child) => ({ index, item: child.className }));
        }));
        expect(misplaced, `${language} ${view} ${width}x${height}: transfer cells stay on one row`).toEqual([]);
        for (const row of await page.locator('.mini-row').all()) {
          await expect(row.locator('.mini-status')).toBeVisible();
          await expect(row.locator('.mini-cancel')).toBeVisible();
        }
        const overflow = await page.evaluate(() => {
          const selectors = ['.player-bar', '.search-form', '.results-pager', '.details-pane', '.full-panel', '.mini-transfers', '.statusbar', '.folder-path', '.actionbar', '.tag-editor', '.profile-card', '.settings-view fieldset', '.mobile-connect-grid'];
          return selectors.flatMap((selector) => [...document.querySelectorAll(selector)].flatMap((node) => {
            const box = node.getBoundingClientRect();
            return box.width && box.height && (node.scrollWidth > node.clientWidth + 2 || box.left < -1 || box.right > innerWidth + 1)
              ? [{ selector, width: node.clientWidth, content: node.scrollWidth }] : [];
          }));
        });
        expect(overflow, `${language} ${view} ${width}x${height}: no horizontal clipping`).toEqual([]);
        const reachable = view === 'Downloads' ? '.tag-editor button' : view === 'Trollbox' ? '.trollbox-compose input'
          : view === 'Settings' ? '.settings-actions button:last-child' : view === 'Profile' ? '.edit-profile button'
          : view === 'Napstrfy' ? '.paired-devices-card' : null;
        if (reachable) {
          const target = page.locator(reachable);
          await target.scrollIntoViewIfNeeded();
          const visible = await target.evaluate((node) => {
            const box = node.getBoundingClientRect(), workspace = document.querySelector('.workspace').getBoundingClientRect();
            return box.bottom > workspace.top && box.top < workspace.bottom;
          });
          expect(visible, `${view} controls can be reached at ${width}x${height}`).toBe(true);
        }
        if (width === 560 && (view === 'Search' || view === 'Downloads' || view === 'Trollbox')) {
          await page.screenshot({ path: test.info().outputPath(`${view}-${language}-560.png`) });
        }
      }
    }
  });
}
