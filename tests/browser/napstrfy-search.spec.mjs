import { test, expect } from '@playwright/test';
import { mockNative, serveAudio } from './helpers/native.mjs';

async function openSearch(page, streamOnly = false) {
  await mockNative(page);
  await page.route('**/fixture.wav', serveAudio);
  await page.addInitScript((streamOnly) => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const pending = new Map();
    window.searchRequests = [];
    window.finishSearch = (cmd, query, tracks, error) => {
      const request = pending.get(`${cmd}:${query}`);
      if (error) request.reject(error);
      else request.resolve(cmd === 'remote_library' ? { tracks, total: tracks.length } : tracks);
    };
    window.__TAURI_INTERNALS__.invoke = async (cmd, args = {}) => {
      if (cmd === 'companion_status' || cmd === 'cached_library') return { ...await invoke(cmd, args), streamOnly };
      if ((cmd === 'remote_library' && args.query) || cmd === 'remote_search') {
        window.searchRequests.push({ cmd, ...args });
        return new Promise((resolve, reject) => pending.set(`${cmd}:${args.query}`, { resolve, reject }));
      }
      return invoke(cmd, args);
    };
  }, streamOnly);
  await page.goto('http://127.0.0.1:15174');
  await expect(page.locator('.track-row')).toHaveCount(1);
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
}

function track(id, local = true) {
  return { fileId: id.repeat(64), filename: `${id}.wav`, title: `Track ${id}`, artist: 'Artist', album: '',
    format: 'WAV', mime: 'audio/wav', size: 1234567, tags: '', local, sources: [] };
}

// Our shell does not put the field in the header: it lives on the second
// `.bottom-nav` tab, so a search always starts by opening that tab.
async function openSearchTab(page) {
  await page.locator('.bottom-nav button').nth(1).click();
  await expect(page.getByRole('textbox', { name: 'Search tracks', exact: true })).toBeVisible();
}

async function search(page, query) {
  await openSearchTab(page);
  const input = page.getByRole('textbox', { name: 'Search tracks', exact: true });
  await input.fill(query);
  await input.press('Enter');
  await expect.poll(() => page.evaluate((query) => window.searchRequests.some((request) => request.query === query), query)).toBe(true);
}

async function finish(page, cmd, query, tracks = [], error) {
  await page.evaluate(({ cmd, query, tracks, error }) => window.finishSearch(cmd, query, tracks, error), { cmd, query, tracks, error });
}

// We have no `.network-search-status` line: the search field spins for as long as
// either half is still out, and the rows carry `aria-busy` alongside it.
const networkPending = (page) => expect(page.locator('.search-spinner')).toBeVisible();

test('Napstrfy full access plays host matches while network search waits, then appends without duplicates or losing selection', async ({ page }) => {
  await openSearch(page);
  await search(page, 'music');
  await finish(page, 'remote_library', 'music', [track('b'), track('c')]);
  await expect(page.locator('.track-row strong')).toHaveText(['Track b', 'Track c']);
  await networkPending(page);
  await page.locator('.track-open').nth(1).click();
  await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
  await finish(page, 'remote_search', 'music', [track('b', false), track('d', false)]);
  await expect(page.locator('.track-row strong')).toHaveText(['Track b', 'Track c', 'Track d']);
  await expect(page.locator('.track-row.remote strong')).toHaveText(['Track d']);
  await expect(page.locator('.track-row.selected strong')).toHaveText('Track c');
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
});

test('Napstrfy keeps host matches when the network search fails', async ({ page }) => {
  await openSearch(page);
  await search(page, 'music');
  await finish(page, 'remote_library', 'music', [track('b')]);
  await finish(page, 'remote_search', 'music', [], 'Network search timed out');
  await expect(page.locator('.track-row strong')).toHaveText(['Track b']);
  // Only the network half failed, so this is the softer "host results only"
  // notice rather than the error banner.
  await expect(page.locator('.toast')).toContainText('Network search timed out');
  await expect(page.locator('.error-banner')).toHaveCount(0);
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
});

test('Napstrfy handles network results arriving before the host-library response', async ({ page }) => {
  await openSearch(page);
  await search(page, 'music');
  await finish(page, 'remote_search', 'music', [track('b', false), track('c', false)]);
  await expect(page.locator('.track-row')).toHaveCount(2);
  await finish(page, 'remote_library', 'music', [track('b')]);
  await expect(page.locator('.track-row strong')).toHaveText(['Track b', 'Track c']);
  await expect(page.locator('.track-row.remote strong')).toHaveText(['Track c']);
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
});

test('Napstrfy read-only search uses only the host library', async ({ page }) => {
  await openSearch(page, true);
  await search(page, 'music');
  await finish(page, 'remote_library', 'music', [track('b')]);
  await expect(page.locator('.track-row strong')).toHaveText(['Track b']);
  await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
  expect(await page.evaluate(() => window.searchRequests.map((request) => request.cmd))).toEqual(['remote_library']);
});

test('Napstrfy does not show an empty result until both searches finish', async ({ page }) => {
  await openSearch(page);
  await search(page, 'music');
  await finish(page, 'remote_library', 'music');
  await expect(page.locator('.empty-library')).toHaveCount(0);
  await networkPending(page);
  await finish(page, 'remote_search', 'music');
  await expect(page.locator('.empty-library')).toBeVisible();
});

for (const action of ['new search', 'clear', 'liked']) {
  test(`Napstrfy ignores late results and errors after ${action}`, async ({ page }) => {
    await openSearch(page);
    await search(page, 'old');
    if (action === 'new search') {
      await search(page, 'new');
      await finish(page, 'remote_library', 'new', [track('c')]);
      await finish(page, 'remote_search', 'new', [track('d', false)]);
    } else if (action === 'clear') await page.locator('.clear-search').click();
    else await page.locator('.chips button').first().click();
    const expected = action === 'new search' ? ['Track c', 'Track d'] : action === 'clear' ? ['Search'] : [];
    await expect(page.locator('.track-row strong')).toHaveText(expected);
    await finish(page, 'remote_library', 'old', [track('b')]);
    await finish(page, 'remote_search', 'old', [], 'Stale search failed');
    await expect(page.locator('.track-row strong')).toHaveText(expected);
    await expect(page.locator('.error-banner')).toHaveCount(0);
    await expect(page.locator('.track-list')).toHaveAttribute('aria-busy', 'false');
  });
}
