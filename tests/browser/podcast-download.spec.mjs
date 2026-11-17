import { test, expect } from '@playwright/test';
import { mockNative } from './helpers/native.mjs';

test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });

async function openEpisodes(page, mode) {
  await mockNative(page, { platform: 'android' });
  await page.addInitScript((mode) => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    window.podcastAttempt = 0;
    window.downloadState = [];
    let failRefresh = false;
    window.__TAURI_INTERNALS__.invoke = async (cmd, args = {}) => {
      if (cmd === 'podcast_download') {
        window.calls.push({ cmd, args });
        window.podcastAttempt++;
        const failed = mode === 'immediate-failure' && window.podcastAttempt === 1;
        window.downloadState = [{ episode: args.episode, ready: false, progress: 0, status: failed ? 'Failed: publisher returned 403 Forbidden' : 'Downloading' }];
        failRefresh = mode === 'refresh-failure';
        return;
      }
      if (cmd === 'podcast_downloads') {
        window.calls.push({ cmd, args });
        if (failRefresh) {
          failRefresh = false;
          throw 'Temporary status refresh failure';
        }
        return window.downloadState;
      }
      return invoke(cmd, args);
    };
  }, mode);
  await page.goto('http://127.0.0.1:15174');
  await page.locator('.app-nav button').nth(1).click();
  await page.locator('.podcast-open').first().click();
  await expect(page.locator('.episode-download')).toBeEnabled();
}

test('podcast background failures remain visible and can be retried', async ({ page }) => {
  await openEpisodes(page, 'immediate-failure');
  const download = page.locator('.episode-download');
  await download.click();
  await expect(page.getByRole('alert')).toContainText('publisher returned 403 Forbidden');
  await expect(page.locator('.notice-banner')).toHaveCount(0);
  await expect(download).toBeEnabled();
  await download.click();
  await expect(page.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '0');
  await expect(page.getByRole('alert')).toHaveCount(0);
  expect(await page.evaluate(() => window.podcastAttempt)).toBe(2);
});

test('podcast progress polling survives a failed first refresh and reaches completion', async ({ page }, testInfo) => {
  await openEpisodes(page, 'refresh-failure');
  const download = page.locator('.episode-download');
  const originalSize = await download.boundingBox();
  await download.click();
  const progress = page.getByRole('progressbar');
  await expect(progress).toHaveAttribute('aria-valuenow', '0');
  const progressSize = await progress.boundingBox();
  expect(progressSize.width).toBe(originalSize.width);
  expect(progressSize.height).toBe(originalSize.height);
  await page.evaluate(() => { window.downloadState[0].progress = 55; });
  await expect(progress).toHaveAttribute('aria-valuenow', '55', { timeout: 8000 });
  await expect(progress.locator('.episode-download-progress')).toHaveAttribute('stroke-dashoffset', '45');
  await page.screenshot({ path: testInfo.outputPath('podcast-progress-android.png') });
  await page.evaluate(() => { Object.assign(window.downloadState[0], { ready: true, progress: 100, status: 'Downloaded' }); });
  await expect(download).toHaveText('✓', { timeout: 8000 });
  await expect(progress).toHaveCount(0);
  await expect(download).toHaveClass(/ready/);
  await expect(download).toHaveAccessibleName('Ready to play: Original episode');
  await page.screenshot({ path: testInfo.outputPath('podcast-complete-android.png') });
  const completedSize = await download.boundingBox();
  expect(completedSize.width).toBe(originalSize.width);
  expect(completedSize.height).toBe(originalSize.height);
  await download.click();
  expect(await page.evaluate(() => window.podcastAttempt)).toBe(1);
});

test('a later podcast download failure is visible without hovering', async ({ page }) => {
  await openEpisodes(page, 'later-failure');
  await page.locator('.episode-download').click();
  await expect(page.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '0');
  await page.evaluate(() => { window.downloadState[0].status = 'Failed: publisher stream timed out'; });
  await expect(page.getByRole('alert')).toContainText('publisher stream timed out', { timeout: 8000 });
  await expect(page.locator('.episode-download')).toBeEnabled();
  await expect(page.locator('.notice-banner')).toHaveCount(0);
});


test('podcast failure details are displayed as text without executing HTML', async ({ page }) => {
  await openEpisodes(page, 'later-failure');
  await page.locator('.episode-download').click();
  await page.evaluate(() => {
    window.downloadState[0].status = 'Failed: <img src=x onerror="window.podcastErrorExecuted=true">';
  });
  const alert = page.getByRole('alert');
  await expect(alert).toContainText('<img src=x onerror="window.podcastErrorExecuted=true">', { timeout: 8000 });
  await expect(alert.locator('img')).toHaveCount(0);
  await expect(page.locator('.error-banner img')).toHaveCount(0);
  expect(await page.evaluate(() => window.podcastErrorExecuted)).toBeUndefined();
});
