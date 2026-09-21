import { test, expect } from '@playwright/test';
import { mockNative, serveAudio } from './helpers/native.mjs';

// Our phone has no scraper: the paired host resolves kind-30427 cover events and
// answers `remote_covers`. These tests play the host, and watch which albums the
// phone asks about and when it asks again.
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64');

/**
 * `count` tracks, each its own album, so every tile has its own cover key. The
 * host answers every key with a pair of renditions, a URL each, which says which
 * album a request was for and which of the two was asked for.
 */
async function seedLibrary(page, count = 40) {
  await page.addInitScript((count) => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const makeCover = (key, index) => ({ key, art: `/cover-${index}.png`, thumb: `/cover-${index}-thumb.png`,
      mbid: '', year: '', genre: '', collection: '', source: 'itunes', coverFileId: '', mime: 'image/png', author: '', seeder: false });
    const indexFor = (key) => Number(key.split('|')[1].slice('album '.length));
    const tracks = Array.from({ length: count }, (_, index) => ({
      fileId: index.toString(16).padStart(64, '0'),
      filename: `Song ${index}.wav`, title: `Song ${index}`, artist: 'Rancid', album: `Album ${index}`,
      format: 'WAV', mime: 'audio/wav', size: 1234567, tags: '', local: true, sources: []
    }));
    window.coverAsks = [];
    window.coverFails = false;
    window.__TAURI_INTERNALS__.invoke = async (cmd, args = {}) => {
      if (cmd === 'cached_library') return { paired: true, connected: true, tracks, total: tracks.length };
      if (cmd === 'remote_library') return { tracks, total: tracks.length };
      if (cmd === 'remote_covers') {
        window.coverAsks.push([...args.keys]);
        if (window.coverFails) throw new Error('Host is not reachable');
        return args.keys.map((key) => makeCover(key, indexFor(key)));
      }
      return invoke(cmd, args);
    };
  }, count);
}

const asks = (page) => page.evaluate(() => window.coverAsks);
const askCount = (page) => page.evaluate(() => window.coverAsks.length);

test('Napstrfy asks the host about the artwork it can show, in batches by album, and paints the player the same way', async ({ page }) => {
  await mockNative(page);
  await seedLibrary(page);
  await page.route('**/cover-*.png', (route) => route.fulfill({ contentType: 'image/png', body: PNG }));
  await page.route('**/fixture.wav', serveAudio);
  await page.goto('http://127.0.0.1:15174');
  const rows = page.locator('.track-row');
  await expect(rows).toHaveCount(40);
  // A row draws the publisher's thumbnail: the largest tile on a screen of rows is
  // a few dozen pixels across.
  await expect(rows.first().locator('.artwork img')).toHaveAttribute('src', '/cover-0-thumb.png');
  // One batched ask for the albums the screen shows, rather than one call per
  // tile. The batch is whatever registered inside the flush window, so how many
  // albums it holds depends on how much of the list had laid out by then: the
  // count is bounded, and no row index is pinned, because "album 39 is not in
  // it" was never something the batching promises. Duplicates are.
  const [first] = await asks(page);
  expect(await askCount(page)).toBe(1);
  expect(first.length).toBeGreaterThan(4);
  expect(first.length).toBeLessThan(40);
  expect(new Set(first).size).toBe(first.length);
  // Scrolling to the far end is what asks for whatever was never asked for. How
  // many batches that takes is timing, not behaviour, so only the fact that new
  // work happened is asserted.
  await rows.nth(39).scrollIntoViewIfNeeded();
  await expect(rows.nth(39).locator('.artwork img')).toHaveAttribute('src', '/cover-39-thumb.png');
  await expect.poll(() => askCount(page)).toBeGreaterThan(1);
  // Playing it draws the same answer in the player, without leaving the screen
  // and - the guarantee worth pinning - without asking the host again. The drawer
  // is the thumbnail standing under the full cover, both of this same album.
  const asksBeforePlay = await askCount(page);
  await rows.nth(39).locator('.track-open').click();
  await expect(page.locator('.now-sheet-art img.now-sheet-art-thumb')).toHaveAttribute('src', '/cover-39-thumb.png');
  await expect(page.locator('.now-sheet-art img.now-sheet-art-full')).toHaveAttribute('src', '/cover-39.png');
  await expect(rows).toHaveCount(40);
  expect(await askCount(page)).toBe(asksBeforePlay);
});

test('Napstrfy asks about the artwork of the track next in the queue while the one before it plays', async ({ page }) => {
  await mockNative(page);
  await seedLibrary(page, 40);
  // Recorded rather than counted: both renditions of the album are asked for, and
  // each has its own URL here so the two can be told apart.
  const fetched = [];
  await page.route('**/cover-*.png', (route) => {
    fetched.push(new URL(route.request().url()).pathname);
    return route.fulfill({ contentType: 'image/png', body: PNG });
  });
  await page.route('**/fixture.wav', serveAudio);
  await page.goto('http://127.0.0.1:15174');
  const rows = page.locator('.track-row');
  await expect(rows).toHaveCount(40);
  await rows.first().locator('.track-open').click();
  // The playlist looks its own artwork up for the first twelve rows only, and the
  // library rows that far down have never been scrolled into view, so the album
  // twenty-one tracks along has never been asked about by anything on screen.
  await page.getByRole('button', { name: 'Open the playlist' }).click();
  const asked = async () => (await asks(page)).flat().join('\n');
  // The rows on screen have asked for their own albums by now, and the ones below
  // the fold have not, so this is a real absence rather than a race not yet lost.
  await expect.poll(asked).toContain('album 5');
  expect(await asked()).not.toContain('album 21');
  expect(fetched).not.toContain('/cover-21-thumb.png');
  expect(fetched).not.toContain('/cover-21.png');
  await page.locator('.queue-row').nth(20).locator('.queue-open').click();
  // Playing it is what asks on behalf of the track that will follow it, and what
  // fetches both of its renditions: the thumbnail so the player has a cover at
  // once, and the full one so the drawer and the lock screen have nothing left to
  // wait for.
  await expect.poll(asked).toContain('album 21');
  await expect.poll(() => fetched).toContain('/cover-21-thumb.png');
  await expect.poll(() => fetched).toContain('/cover-21.png');
});

test('Napstrfy artwork recovers from a transient host failure while the screen stays open', async ({ page }) => {
  await mockNative(page);
  await seedLibrary(page, 1);
  await page.addInitScript(() => { window.coverFails = true; });
  await page.clock.install();
  await page.route('**/cover-*.png', (route) => route.fulfill({ contentType: 'image/png', body: PNG }));
  await page.goto('http://127.0.0.1:15174');
  await page.clock.runFor(500);
  await expect(page.locator('.track-row .artwork img')).toHaveClass('fallback');
  expect(await askCount(page)).toBe(1);
  // A request that failed is not an answer, so the tile asks again on its own
  // bounded retry rather than accepting that the album has no cover.
  await page.evaluate(() => { window.coverFails = false; });
  await page.clock.runFor(61_000);
  await page.clock.runFor(500);
  await expect(page.locator('.track-row .artwork img')).toHaveAttribute('src', '/cover-0-thumb.png');
  expect(await askCount(page)).toBe(2);
  expect(await page.locator('.track-row').count()).toBe(1);
});

test('Napstrfy artwork gives up after three tries, so a dead host is not polled forever', async ({ page }) => {
  await mockNative(page);
  await seedLibrary(page, 1);
  await page.addInitScript(() => { window.coverFails = true; });
  await page.clock.install();
  await page.goto('http://127.0.0.1:15174');
  for (let round = 0; round < 5; round += 1) {
    await page.clock.runFor(61_000);
    await page.clock.runFor(500);
  }
  await expect(page.locator('.track-row .artwork img')).toHaveClass('fallback');
  expect(await askCount(page)).toBe(3);
});
