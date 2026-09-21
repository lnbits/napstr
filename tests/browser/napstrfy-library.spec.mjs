import { test, expect } from '@playwright/test';
import { mockNative, serveAudio } from './helpers/native.mjs';

// The host answers an album query by name, because the companion protocol has no
// album-level artist field to ask with. These tests pin down what the phone is
// entitled to do with that answer.
const id = (letter) => letter.repeat(64);
// A 1x1 PNG, so the header's two renditions are real images to the browser.
const tinyPng = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==',
  'base64'
);
const coverThumb = 'https://example.com/cover-thumb.jpg';
const coverFull = 'https://example.com/cover-full.jpg';
const track = (letter, title, artist, album, local = true) => ({
  fileId: id(letter), filename: `${title}.wav`, title, artist, album,
  format: 'WAV', mime: 'audio/wav', size: 1234567, tags: '', local, sources: []
});

// "Doubleback" is on ZZ Top's Greatest Hits. Linkin Park and Will Smith have
// albums of that name too, which is how the two ended up in one view.
const zzTop = [
  track('a', 'Doubleback', 'ZZ Top', 'Greatest Hits'),
  track('b', 'Gimme All Your Lovin', 'ZZ Top', 'Greatest Hits'),
  track('c', 'Sharp Dressed Man', 'ZZ Top', 'Greatest Hits')
];
const sameTitleOtherArtists = [
  track('d', 'Numb', 'Linkin Park', 'Greatest Hits'),
  track('e', 'Gettin Jiggy Wit It', 'Will Smith', 'Greatest Hits')
];
const likedSong = track('f', 'Liked song', 'Artist', '');
// The cover carries the year, and it arrives in the same batch as the album's
// tracks, so its appearance is how this test knows the host has answered.
const albumCover = {
  key: 'zz top|greatest hits', art: coverFull, thumb: coverThumb, mbid: '',
  year: '1979', genre: '', collection: '', source: 'manual', coverFileId: '', mime: '',
  author: '', eventId: '', createdAt: 0, seeder: false
};

async function openApp(page, { library = [], album = null, cached = null, likes = null, platform = 'linux', holdFullCover = false, recordMedia = false } = {}) {
  // A test that needs the full rendition still in flight holds it there, rather
  // than racing the clock: the player bar fetches that same image, so a delay
  // can expire before the drawer that is being tested is even open.
  let releaseFullCover = () => {};
  const fullCoverGate = holdFullCover ? new Promise((resolve) => { releaseFullCover = resolve; }) : null;
  await mockNative(page, { platform });
  await page.route('**/fixture.wav', serveAudio);
  // Registered after the blanket https route, so it wins for the covers. The
  // full rendition is held back to leave the thumbnail on screen on its own.
  await page.route(coverThumb, (route) => route.fulfill({ contentType: 'image/png', body: tinyPng }));
  await page.route(coverFull, async (route) => {
    if (fullCoverGate) await fullCoverGate;
    else await new Promise((resolve) => setTimeout(resolve, 400));
    await route.fulfill({ contentType: 'image/png', body: tinyPng });
  });
  await page.addInitScript(({ library, album, cover, cached, likes, recordMedia }) => {
    if (likes) window.localStorage.setItem('napstrfy-liked-music', JSON.stringify(likes));
    if (recordMedia) {
      // The media service is not in the browser either: keep what it is told, so a
      // test can watch the lock screen's artwork arrive and then be replaced.
      window.mediaUpdates = [];
      window.NapstrfyMedia = { update: (payload) => window.mediaUpdates.push(JSON.parse(payload)), clear: () => {} };
    }
    // The page talks to Kotlin through this object. The native side is not in the
    // browser, so record the flag it is given instead of pressing a real button,
    // and count what Android would have done with a press it was not given.
    window.backAvailable = null;
    window.appExits = 0;
    window.NapstrfyBack = {
      setBackAvailable: (available) => { window.backAvailable = available; },
      setDrawerOpen: (open) => { window.backAvailable = open; }
    };
    const invoke = window.__TAURI_INTERNALS__.invoke;
    window.__TAURI_INTERNALS__.invoke = async (cmd, args = {}) => {
      // `cached` is the phone's own cache, which is what the badge reads.
      if (cached && cmd === 'cached_library') return { tracks: cached, total: cached.length };
      if (album && cmd === 'remote_library' && args.query === 'Greatest Hits') {
        return { tracks: album, total: album.length };
      }
      if (album && cmd === 'remote_covers') return [cover];
      if (cmd === 'remote_library' && !args.query) return { tracks: library, total: library.length };
      return invoke(cmd, args);
    };
  }, { library, album, cover: albumCover, cached, likes, recordMedia });
  await page.goto('http://127.0.0.1:15174');
  return { releaseFullCover };
}

/** The liked page is only reachable from the chip on the search tab. */
async function openLikedPage(page) {
  await page.locator('.bottom-nav button').nth(1).click();
  await page.locator('.chips button').first().click();
  await expect(page.locator('.library-heading h1')).toHaveText('Liked music');
  await expect(page.locator('.track-row strong')).toHaveText(['Liked song']);
}

const searchPageIsBack = (page) => Promise.all([
  expect(page.getByRole('textbox', { name: 'Search tracks', exact: true })).toBeVisible(),
  expect(page.locator('.library-heading h1')).toHaveText('Find something'),
  expect(page.locator('.liked-close')).toHaveCount(0)
]);

const backFlag = (page) => page.evaluate(() => window.backAvailable);

/**
 * Presses the hardware back button the way Android does. The page is given the
 * press only while it advertises a destination, and the flag is taken when the
 * press is consumed - which is the page's cue to publish its answer again. A
 * press with nothing behind it leaves the app.
 */
async function pressBack(page) {
  return page.evaluate(() => {
    if (!window.backAvailable) {
      window.appExits += 1;
      return 'exit';
    }
    window.backAvailable = false;
    window.dispatchEvent(new CustomEvent('napstrfy-back'));
    return 'handled';
  });
}

test('Napstrfy leaves the liked page from its close button', async ({ page }) => {
  await openApp(page, { likes: [likedSong] });
  await openLikedPage(page);
  await page.locator('.liked-close').click();
  await searchPageIsBack(page);
});

test('Napstrfy walks back out of every page it advertises, and is only left from home', async ({ page }) => {
  // A phone, because a wide desktop window pins the player as a column that is
  // never a drawer to close.
  await openApp(page, { library: zzTop, album: zzTop, likes: [likedSong], platform: 'android' });
  // Home has nothing behind it, so this is the one press that leaves the app.
  await expect.poll(() => backFlag(page)).toBe(false);
  expect(await pressBack(page)).toBe('exit');

  // The search tab has home behind it.
  await page.locator('.bottom-nav button').nth(1).click();
  await expect.poll(() => backFlag(page)).toBe(true);
  expect(await pressBack(page)).toBe('handled');
  await expect(page.locator('.library-heading h1')).toHaveText('Your music');

  // The liked page has the search page behind it, and the press after this one is
  // the one that used to leave the app: the flag is taken when a press is
  // consumed, and the liked page closing onto another page that back can leave
  // does not change the answer, so the page has to publish it again regardless.
  await openLikedPage(page);
  expect(await pressBack(page)).toBe('handled');
  await searchPageIsBack(page);
  expect(await pressBack(page)).toBe('handled');
  await expect(page.locator('.library-heading h1')).toHaveText('Your music');

  // An album preview, and the player drawer, each have the library behind them.
  await page.locator('.album-open').click();
  await expect.poll(() => backFlag(page)).toBe(true);
  expect(await pressBack(page)).toBe('handled');
  await expect(page.locator('.album-view')).toHaveCount(0);

  await page.locator('.track-open').first().click();
  await page.locator('.now-open').click();
  await expect.poll(() => backFlag(page)).toBe(true);
  expect(await pressBack(page)).toBe('handled');
  await expect(page.locator('.now-sheet')).toHaveCount(0);

  // One press in the whole walk was not the page's to handle.
  expect(await page.evaluate(() => window.appExits)).toBe(1);
});

test('Napstrfy draws the player bar and the cover stretched behind it from the thumbnail', async ({ page }) => {
  // The full rendition is never delivered in this test, so whatever is drawn on
  // the bar is drawn from the small one the tile that was tapped already had.
  const { releaseFullCover } = await openApp(page, { library: zzTop, album: zzTop, platform: 'android', holdFullCover: true });
  await page.locator('.track-open').first().click();
  await expect(page.locator('.now-playing .artwork img')).toHaveAttribute('src', coverThumb);
  const bar = await page.locator('.now-playing').getAttribute('style');
  expect(bar).toContain(coverThumb);
  expect(bar).not.toContain(coverFull);
  releaseFullCover();
});

test('Napstrfy gives the lock screen the thumbnail, then the full cover once it has landed', async ({ page }) => {
  // The full rendition is held open, so the order the two are published in is the
  // assertion rather than a race between them.
  const { releaseFullCover } = await openApp(page, { library: zzTop, album: zzTop, platform: 'android', recordMedia: true, holdFullCover: true });
  await page.locator('.track-open').first().click();
  const artwork = () => page.evaluate(() => window.mediaUpdates.at(-1)?.artwork ?? '');
  // What the lock screen is given while the bigger rendition is on its way is the
  // one already on this phone.
  await expect.poll(artwork).toBe(coverThumb);
  releaseFullCover();
  // The full cover takes its place as soon as it has loaded, which the media
  // service takes as a new URL for the same art.
  await expect.poll(artwork).toBe(coverFull);
});

test('Napstrfy leaves the liked page with a right swipe, without playing what was under the finger', async ({ page }) => {
  await openApp(page, { likes: [likedSong] });
  await openLikedPage(page);
  const box = await page.locator('.track-list').boundingBox();
  await page.mouse.move(box.x + 30, box.y + 40);
  await page.mouse.down();
  await page.mouse.move(box.x + 130, box.y + 42, { steps: 4 });
  await page.mouse.move(box.x + 300, box.y + 44, { steps: 4 });
  await page.mouse.up();
  await searchPageIsBack(page);
  // The pull ended on a track row, whose click starts playback: it must not.
  expect(await page.evaluate(() => window.calls.filter((call) => call.cmd === 'cache_remote_audio').length)).toBe(0);
});

test('Napstrfy keeps one artist\'s Greatest Hits out of another artist\'s', async ({ page }) => {
  await openApp(page, { library: zzTop, album: [...zzTop, ...sameTitleOtherArtists] });
  await expect(page.locator('.album-card strong')).toHaveText('Greatest Hits');
  await page.locator('.album-open').click();
  // Wait for the host's album answer rather than the shelf's own three rows,
  // which are on screen before the request even goes out.
  await expect(page.locator('.album-meta')).toContainText('1979');
  // The three ZZ Top tracks, and none of the two other artists' same-named album.
  await expect(page.locator('.album-tracks li')).toHaveCount(3);
  await expect(page.locator('.album-track-copy small')).toHaveText(['ZZ Top', 'ZZ Top', 'ZZ Top']);
  await expect(page.locator('.album-track-copy strong')).toHaveText(['Doubleback', 'Gimme All Your Lovin', 'Sharp Dressed Man']);
});

test('Napstrfy moves the storage badge when playing puts a track on the phone', async ({ page }) => {
  await openApp(page, { library: [zzTop[0]], cached: [] });
  const badge = page.locator('.track-list .track-badge');
  await expect(badge).toHaveClass(/computer/);
  await page.locator('.track-open').click();
  // The host caches the file for offline playback, so it is on this phone now.
  await expect(badge).toHaveClass(/phone/);
});

test('Napstrfy opens an album on its thumbnail and fades the full cover in over it', async ({ page }) => {
  await openApp(page, { library: zzTop, album: zzTop });
  await page.locator('.album-open').click();
  // The small rendition is what the shelf tile already fetched, so it is up
  // before the full cover has finished, and it is what the blurred glow uses.
  const backdrop = page.locator('.album-art-backdrop');
  await expect(backdrop).toHaveAttribute('src', coverThumb);
  // The glow is blurred past the point where a full cover would show, so it must
  // be the small rendition rather than a second fetch of the big one.
  const glowStyle = await page.locator('.album-glow').getAttribute('style');
  expect(glowStyle).toContain(coverThumb);
  expect(glowStyle).not.toContain(coverFull);
  const full = page.locator('.album-art-full');
  await expect(full).not.toHaveClass(/ready/);
  await expect(full).toHaveClass(/ready/);
});

test('Napstrfy opens the now-playing drawer on the thumbnail rather than a blank square', async ({ page }) => {
  // A phone, because a wide desktop window pins this sheet as a column and has
  // no drawer to open.
  const { releaseFullCover } = await openApp(page, { library: zzTop, album: zzTop, platform: 'android', holdFullCover: true });
  await page.locator('.track-open').first().click();
  await page.locator('.now-open').click();
  const thumb = page.locator('.now-sheet-art img.now-sheet-art-thumb');
  const full = page.locator('.now-sheet-art img.now-sheet-art-full');
  // The tile that was tapped fetched the small rendition, so the drawer is a
  // cover from its first frame while the full one is still on its way.
  await expect(thumb).toHaveAttribute('src', coverThumb);
  await expect(full).not.toHaveClass(/ready/);
  releaseFullCover();
  await expect(full).toHaveClass(/ready/);
  // The backdrop is blurred too far to show a bigger image, so it takes the
  // small rendition rather than making a second request for the large one.
  const backdrop = await page.locator('.now-sheet-backdrop').getAttribute('style');
  expect(backdrop).toContain(coverThumb);
  expect(backdrop).not.toContain(coverFull);
});
