import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { languages, direction } from '../../shared/i18n/core.js';
import { mockNative, serveAudio, silentAudio } from './helpers/native.mjs';
const catalogs = Object.fromEntries(await Promise.all(languages.map(async ({ code }) => [code, JSON.parse(await readFile(new URL(`../../shared/i18n/locales/${code}.json`, import.meta.url)))])));

for (const platform of ['android', 'linux']) {
  test(`Napstrfy ${platform}: opening screen has a readable language selector without a dialog`, async ({ page }) => {
    await mockNative(page, { paired: false, platform });
    await page.setViewportSize({ width: platform === 'android' ? 360 : 1100, height: 900 });
    await page.goto('http://127.0.0.1:15174');
    const selector = page.locator('.pair-screen [data-language-select]');
    await expect(selector).toBeVisible();
    await expect.poll(() => page.locator('.pair-logo img').evaluate((image) => image.naturalWidth)).toBe(512);
    if (platform === 'linux') await expect(page.locator('.pair-logo img')).toHaveCSS('width', '128px');
    await expect(page.locator('dialog[open]')).toHaveCount(0);
    await expect(page.locator('.pair-screen button').filter({ hasText: /^Settings$/ })).toHaveCount(0);
    await selector.selectOption('fr');
    await expect(selector).toHaveCSS('color', 'rgb(0, 0, 0)');
    await expect(selector).toHaveCSS('background-color', 'rgb(244, 244, 245)');
    await page.reload();
    await expect(selector).toHaveValue('fr');
    await page.screenshot({ path: test.info().outputPath(`${platform}-opening-language.png`) });
  });

  test(`Napstrfy ${platform}: 15-second controls seek safely and system controls use live playback position`, async ({ page }) => {
    await mockNative(page, { platform });
    await page.addInitScript((platform) => {
      window.mediaUpdates = [];
      window.mediaHandlers = {};
      if (platform === 'android') {
        window.NapstrfyMedia = { update: (payload) => window.mediaUpdates.push(JSON.parse(payload)), clear: () => {} };
      } else {
        navigator.mediaSession.setActionHandler = (action, handler) => { window.mediaHandlers[action] = handler; };
      }
    }, platform);
    await page.route('**/fixture.wav', serveAudio);
    await page.setViewportSize({ width: platform === 'android' ? 320 : 1100, height: 800 });
    await page.goto('http://127.0.0.1:15174');
    const back = page.getByRole('button', { name: 'Back 15 seconds', exact: true });
    const forward = page.getByRole('button', { name: 'Forward 15 seconds', exact: true });
    await expect(back).toBeDisabled();
    await expect(forward).toBeDisabled();
    await page.locator('.track-open').first().click();
    const audio = page.locator('audio');
    await expect.poll(() => audio.evaluate((audio) => audio.paused)).toBe(false);
    await page.locator('.play-main').click();
    await expect.poll(() => audio.evaluate((audio) => audio.paused)).toBe(true);
    await expect(back).toBeEnabled();
    const source = await audio.getAttribute('src');
    await audio.evaluate((audio) => { audio.currentTime = 20; });
    await forward.click();
    await expect.poll(() => audio.evaluate((audio) => audio.currentTime)).toBe(35);
    await back.click();
    await expect.poll(() => audio.evaluate((audio) => audio.currentTime)).toBe(20);
    await audio.evaluate((audio) => { audio.currentTime = 5; });
    await back.click();
    await expect.poll(() => audio.evaluate((audio) => audio.currentTime)).toBe(0);
    await audio.evaluate((audio) => { audio.currentTime = 55; });
    await forward.click();
    await expect.poll(() => audio.evaluate((audio) => audio.currentTime)).toBe(60);
    // Dispatch immediately after changing time: no UI timeupdate can intervene.
    for (const [action, expected] of [['rewind', 15], ['forward', 45]]) {
      const actual = await page.evaluate(({ platform, action }) => {
        const audio = document.querySelector('audio');
        audio.currentTime = 30;
        if (platform === 'android') dispatchEvent(new CustomEvent('napstrfy-media-action', { detail: action }));
        else window.mediaHandlers[action === 'rewind' ? 'seekbackward' : 'seekforward']({});
        return audio.currentTime;
      }, { platform, action });
      expect(actual).toBe(expected);
    }
    expect(await audio.evaluate((audio) => audio.paused)).toBe(true);
    await expect(audio).toHaveAttribute('src', source);
    expect(await page.evaluate(() => window.calls.filter((call) => call.cmd === 'cache_remote_audio').length)).toBe(1);
    if (platform === 'android') {
      await expect.poll(() => page.evaluate(() => window.mediaUpdates.at(-1).canSeek)).toBe(true);
      expect(await page.evaluate(() => window.mediaUpdates.at(-1).labels.rewind)).toBe('Back 15 seconds');
    }
    for (const width of [320, 600, 800, 1100]) {
      await page.setViewportSize({ width, height: 800 });
      const buttons = page.locator('.player-buttons button');
      expect((await buttons.evaluateAll((buttons) => buttons.slice(0, 5).map((button) => button.getAttribute('aria-label')))))
        .toEqual(['Previous track', 'Back 15 seconds', 'Play', 'Forward 15 seconds', 'Next track']);
      let right = 0;
      for (const button of (await buttons.all()).slice(0, 5)) {
        await expect(button).toBeInViewport();
        const box = await button.boundingBox();
        expect(box.x).toBeGreaterThanOrEqual(right);
        right = box.x + box.width;
      }
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      if (width === 320) await page.screenshot({ path: test.info().outputPath(`${platform}-mobile-seek-controls.png`) });
    }
    await page.screenshot({ path: test.info().outputPath(`${platform}-seek-controls.png`) });
  });
}

async function instrumentTiming(page) {
  await page.addInitScript(() => {
    window.timingStats = { reads: 0, ranges: 0, positions: [], metadata: 0 };
    window.reportedDuration = NaN;
    const descriptor = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'duration');
    Object.defineProperty(HTMLMediaElement.prototype, 'duration', {
      configurable: true,
      get() {
        window.timingStats.reads++;
        return window.reportedDuration === 'actual' ? descriptor.get.call(this) : window.reportedDuration;
      }
    });
    Object.defineProperty(HTMLMediaElement.prototype, 'seekable', {
      configurable: true,
      get() { window.timingStats.ranges++; throw new Error('Do not query native seekable ranges'); }
    });
    Object.defineProperty(navigator.mediaSession, 'metadata', {
      configurable: true,
      set() { window.timingStats.metadata++; }
    });
    navigator.mediaSession.setPositionState = (state) => window.timingStats.positions.push({ at: Date.now(), state });
  });
}

for (const platform of ['android', 'linux']) {
  test(`Napstrfy ${platform}: late duration recovers without range queries or playback changes`, async ({ page }) => {
    await mockNative(page, { platform });
    await instrumentTiming(page);
    await page.route('**/fixture.wav', serveAudio);
    await page.goto('http://127.0.0.1:15174');
    await page.locator('.track-open').first().click();
    const audio = page.locator('audio');
    const timeline = page.getByRole('slider', { name: 'Playback position' });
    await expect.poll(() => audio.evaluate((a) => a.paused)).toBe(false);
    await page.locator('.play-main').click();
    await expect(timeline).toBeDisabled();
    await expect(page.locator('.timeline span')).toContainText('/ —');
    expect(await page.evaluate(() => window.timingStats.positions.length)).toBe(0);
    // Duration becomes available without any new metadata event.
    await page.evaluate(() => { window.reportedDuration = 'actual'; });
    await expect(timeline).toBeEnabled();
    await expect(timeline).toHaveAttribute('max', '60');
    await timeline.fill('30');
    await page.getByRole('button', { name: 'Forward 15 seconds', exact: true }).click();
    await expect.poll(() => audio.evaluate((a) => a.currentTime)).toBe(45);
    expect(await audio.evaluate((a) => a.paused)).toBe(true);
    await expect.poll(() => page.evaluate(() => window.timingStats.metadata)).toBe(1);
    const before = await page.evaluate(() => ({ reads: window.timingStats.reads, positions: window.timingStats.positions.length }));
    const immediate = await page.evaluate(() => {
      for (let i = 0; i < 10000; i++) {
        document.querySelector('audio').dispatchEvent(new Event('durationchange'));
        document.querySelector('audio').dispatchEvent(new Event('loadedmetadata'));
      }
      return { reads: window.timingStats.reads, positions: window.timingStats.positions.length };
    });
    expect(immediate).toEqual(before);
    await expect.poll(() => page.evaluate(() => window.timingStats.reads)).toBeGreaterThan(before.reads);
    const stats = await page.evaluate(() => window.timingStats);
    expect(stats.reads - before.reads).toBe(1);
    expect(stats.ranges).toBe(0);
    expect(stats.metadata).toBe(1);
    for (let i = 1; i < stats.positions.length; i++) expect(stats.positions[i].at - stats.positions[i - 1].at).toBeGreaterThanOrEqual(990);
    expect(await page.evaluate(() => window.calls.filter((c) => c.cmd === 'cache_remote_audio').length)).toBe(1);
  });
}

test('Napstrfy podcast duration hints stay estimates and cannot enable seeking or reach system controls', async ({ page }) => {
  await mockNative(page);
  await instrumentTiming(page);
  await page.route('**/fixture.wav', serveAudio);
  await page.goto('http://127.0.0.1:15174');
  await page.locator('.app-nav button').nth(1).click();
  await page.locator('.podcast-open').first().click();
  await page.locator('.episode-copy').first().click();
  const timeline = page.getByRole('slider', { name: 'Playback position' });
  await expect.poll(() => page.locator('audio').evaluate((a) => a.paused)).toBe(false);
  await page.locator('.play-main').click();
  await expect(timeline).toBeDisabled();
  await expect(page.locator('.timeline span')).toContainText('/ ≈ 1:40');
  expect(await page.evaluate(() => window.timingStats.positions.length)).toBe(0);
  await page.evaluate(() => { window.reportedDuration = Number.MAX_VALUE; document.querySelector('audio').dispatchEvent(new Event('durationchange')); });
  await expect.poll(() => page.evaluate(() => window.timingStats.reads)).toBeGreaterThan(1);
  await expect(timeline).toBeDisabled();
  expect(await page.evaluate(() => window.timingStats.positions.length)).toBe(0);
  await page.evaluate(() => { window.reportedDuration = 'actual'; document.querySelector('audio').dispatchEvent(new Event('loadedmetadata')); });
  await expect(timeline).toBeEnabled();
  await expect(page.locator('.timeline span')).toContainText('/ 1:00');
  // The next source must not inherit either the estimate or verified length.
  await page.evaluate(() => { window.reportedDuration = NaN; });
  await page.locator('.app-nav button').first().click();
  await page.locator('.track-open').first().click();
  await expect(timeline).toBeDisabled();
  await expect(page.locator('.timeline span')).toContainText('/ —');
  expect(await page.evaluate(() => window.timingStats.ranges)).toBe(0);
});

test('Napstrfy banners expire after ten seconds and replacement errors get a fresh timer', async ({ page }) => {
  await mockNative(page, { remote: true });
  await page.clock.install();
  await page.goto('http://127.0.0.1:15174');
  await page.locator('.track-open').first().click();
  await expect(page.locator('.notice-banner')).toContainText('Napstr is downloading');
  await page.clock.fastForward(9000);
  await expect(page.locator('.notice-banner')).toBeVisible();
  await page.clock.fastForward(1100);
  await expect(page.locator('.notice-banner')).toHaveCount(0);
  const search = page.getByRole('textbox', { name: 'Search tracks', exact: true });
  for (const message of ['First search failed', 'Second search failed']) {
    await page.evaluate((message) => { window.searchError = message; }, message);
    await search.fill(message);
    await search.press('Enter');
    await expect(page.locator('.error-banner')).toContainText(message);
    if (message.startsWith('First')) await page.clock.fastForward(9000);
  }
  await page.clock.fastForward(2000);
  await expect(page.locator('.error-banner')).toContainText('Second search failed');
  await page.clock.fastForward(8100);
  await expect(page.locator('.error-banner')).toHaveCount(0);
  await search.press('Enter');
  await expect(page.locator('.error-banner')).toBeVisible();
  await page.locator('.error-banner').click();
  await expect(page.locator('.error-banner')).toHaveCount(0);
});

test('Napstrfy desktop player occupies its own column with artwork, likes and playback modes', async ({ page }) => {
  await mockNative(page);
  await page.addInitScript(() => {
    localStorage.setItem('napstrfy-artwork:v2:' + JSON.stringify(['album', 'settings', 'useralbum']), JSON.stringify({ url: '/napstr-logo-small.png', expires: Date.now() + 86400000 }));
  });
  await page.route('**/fixture.wav', serveAudio);
  await page.goto('http://127.0.0.1:15174');
  await page.locator('.track-open').first().click();
  await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
  await page.locator('.play-main').click();
  const cover = page.locator('.now-playing .artwork img');
  await expect(cover).toHaveAttribute('src', '/napstr-logo-small.png');
  await expect(page.locator('.player-backdrop span')).toHaveCSS('background-image', /napstr-logo-small\.png/);
  await expect(page.locator('.player-backdrop span')).toHaveCSS('filter', 'blur(20px)');
  const like = page.locator('.player-like');
  await like.click();
  await expect(like).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.track-row .like-button')).toHaveAccessibleName('Unlike Search');
  await page.locator('.mode-button').click();
  await expect(page.locator('.mode-label')).toHaveText('Play random');
  expect(await page.evaluate(() => localStorage.getItem('napstrfy-play-mode'))).toBe('random');
  for (const width of [800, 1100, 1600]) {
    await page.setViewportSize({ width, height: 900 });
    const nav = await page.locator('.app-nav').boundingBox();
    const content = await page.locator('.app-content').boundingBox();
    const player = await page.locator('.now-playing').boundingBox();
    const art = await page.locator('.now-playing .artwork').boundingBox();
    expect(player.width).toBeCloseTo(nav.width * 1.5);
    expect(content.x).toBeGreaterThanOrEqual(nav.x + nav.width);
    expect(content.x + content.width).toBeLessThanOrEqual(player.x);
    expect(player.x + player.width).toBe(width);
    expect(art.width).toBeGreaterThan(200);
    expect(art.width).toBeCloseTo(art.height);
    expect(art.x).toBeGreaterThan(player.x);
    await expect(page.locator('.now-playing')).toHaveCSS('position', 'relative');
    await expect(page.locator('.mode-button')).toBeInViewport();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: test.info().outputPath(`desktop-player-${width}.png`) });
  }
  await page.setViewportSize({ width: 360, height: 800 });
  await expect(like).toBeHidden();
  await expect(page.locator('.player-backdrop')).toBeHidden();
  await expect(page.locator('.now-playing')).toHaveCSS('position', 'fixed');
  await expect(page.locator('audio')).toHaveCount(1);
  expect(await page.evaluate(() => window.calls.filter((call) => call.cmd === 'cache_remote_audio').length)).toBe(1);
});

for (const app of ['napstr', 'napstrfy']) {
  test(`${app}: all ten languages, saved preference, user content and status behavior`, async ({ page }) => {
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await mockNative(page, { app });
    await page.goto(`http://127.0.0.1:${app === 'napstr' ? 15173 : 15174}`);
    if (app === 'napstr') await page.locator('.tool-button').filter({ hasText: /Settings$/ }).click();
    else await page.locator('.app-nav button').last().click();
    const selector = page.locator('[data-language-select]');
    await expect(selector).toBeVisible();
    for (const { code } of languages) {
      await selector.selectOption(code);
      await expect(page.locator('html')).toHaveAttribute('lang', code);
      await expect(page.locator('html')).toHaveAttribute('dir', direction(code));
      await expect(page.locator('.language-select > span')).toHaveText(catalogs[code].Language);
      if (['ar', 'ur', 'hi', 'bn', 'zh'].includes(code)) {
        await page.evaluate(() => document.fonts.ready);
        expect(await page.evaluate(() => [...document.fonts].some((font) => font.family.startsWith('Noto Sans') && font.status === 'loaded'))).toBe(true);
      }
      expect(await page.evaluate(() => localStorage.getItem('napstr-language'))).toBe(code);
    }
    await page.screenshot({ path: test.info().outputPath(`${app}-urdu-settings.png`) });
    if (app === 'napstr') {
      await page.locator('.tool-button').filter({ hasText: catalogs.ur.Downloads }).click();
      await expect(page.locator('.downloads-view .transfer-play')).toBeEnabled();
      await expect(page.locator('.downloads-view')).toContainText('Downloading');
      await expect(page.locator('.downloads-view')).toContainText('Search.wav');
      await page.locator('.tool-button').filter({ hasText: catalogs.ur.Search }).click();
      await page.locator('#format').selectOption('Audiobooks');
      expect(await page.locator('#format').inputValue()).toBe('Audiobooks');
      await expect.poll(() => page.evaluate(() => window.calls.some((call) => call.cmd === 'network_search_audiobooks'))).toBeTruthy();
    } else {
      await page.keyboard.press('Escape');
      await expect(page.locator('.track-copy strong').first()).toHaveText('Search');
      await page.locator('.app-nav button').nth(1).click();
      await page.locator('.podcast-open').first().click();
      const progress = page.getByRole('progressbar');
      await expect(progress).toHaveAttribute('aria-valuenow', '20');
      await expect(progress).toHaveAttribute('aria-valuetext', `${catalogs.ur['Downloading…']} 20%`);
      await expect(page.locator('button.episode-download')).toHaveCount(0);
    }
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('lang', 'ur');
    expect(errors).toEqual([]);
  });
}

test('native language wins in automatic mode; later native responses cannot overwrite a manual choice', async ({ page }) => {
  await mockNative(page, { nativeLocale: 'ar-SA', paired: false });
  await page.goto('http://127.0.0.1:15174');
  await expect(page.locator('html')).toHaveAttribute('lang', 'ar');
  await page.locator('.pair-screen [data-language-select]').selectOption('es');
  await page.evaluate(() => { window.nativeLocale = 'fr-FR'; dispatchEvent(new Event('focus')); });
  await expect(page.locator('html')).toHaveAttribute('lang', 'es');
  await page.locator('[data-language-select]').selectOption('auto');
  await expect(page.locator('html')).toHaveAttribute('lang', 'fr');
  await page.keyboard.press('Escape');
  await page.locator('textarea').fill('napstrfy://pair/test');
  await page.locator('.manual-pair button[type="submit"]').click();
  await expect(page.locator('.track-row')).toBeVisible();
});

test('browser fallback works without native locale and language selection survives blocked storage', async ({ browser }) => {
  const context = await browser.newContext({ locale: 'pt-BR' });
  const page = await context.newPage();
  await mockNative(page, { nativeLocale: null, paired: false, blockedStorage: true });
  await page.goto('http://127.0.0.1:15174');
  await expect(page.locator('html')).toHaveAttribute('lang', 'pt');
  await page.locator('.pair-screen [data-language-select]').selectOption('hi');
  await expect(page.locator('html')).toHaveAttribute('lang', 'hi');
  await context.close();
});

for (const code of ['ar', 'ur', 'fr', 'bn']) {
  test(`Napstrfy ${code}: mobile and desktop layouts fit without duplicating navigation`, async ({ page }) => {
    await mockNative(page, { saved: code });
    await page.goto('http://127.0.0.1:15174');
    await expect(page.locator('.track-row')).toBeVisible();
    for (const width of [360, 430, 800, 1180]) {
      await page.setViewportSize({ width, height: 810 });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
      await expect(page.locator('.app-nav')).toHaveCount(1);
      const nav = await page.locator('.app-nav').boundingBox();
      if (width >= 800 && ['ar', 'ur'].includes(code)) expect(Math.round(nav.x + nav.width)).toBe(width);
      await page.locator('.app-nav button').last().click();
      await expect(page.locator('[data-language-select]')).toBeInViewport();
      await page.keyboard.press('Escape');
    }
  });
}

test('website suggests language without redirecting; explicit changes preserve project paths, query and anchors', async ({ browser }) => {
  const context = await browser.newContext({ locale: 'es-MX' });
  const page = await context.newPage();
  await page.goto('http://127.0.0.1:15175/preview/index.html?from=shared#how-it-works');
  await expect(page.locator('[data-language-suggestion]')).toBeVisible();
  await expect(page).toHaveURL(/\/preview\/index.html\?from=shared#how-it-works$/);
  await expect(page.locator('[data-suggested-language]')).toHaveText('Español');
  await page.locator('[data-suggested-language]').click();
  await expect(page).toHaveURL(/\/preview\/es\/index.html\?from=shared#how-it-works$/);
  await expect(page.locator('html')).toHaveAttribute('lang', 'es');
  await expect(page.locator('[data-language-select]')).toHaveCount(0);
  await page.locator('.language-links summary').click();
  await page.locator('.language-links a[lang="ur"]').click();
  await expect(page).toHaveURL(/\/preview\/ur\/index.html\?from=shared#how-it-works$/);
  await expect(page.locator('html')).toHaveAttribute('dir', 'rtl');
  await page.goto('http://127.0.0.1:15175/preview/tor.html');
  await expect(page.locator('[data-suggested-language]')).toHaveText('اردو');
  await expect(page).toHaveURL(/\/preview\/tor.html$/);
  await page.locator('[data-suggested-language]').click();
  await expect(page).toHaveURL(/\/preview\/ur\/tor.html$/);
  await page.locator('.language-links summary').click();
  await page.locator('.language-links a[lang="en"]').click();
  await expect(page).toHaveURL(/\/preview\/tor.html$/);
  await context.close();
});

test('website language links and sharing metadata work without JavaScript', async ({ browser }) => {
  const context = await browser.newContext({ javaScriptEnabled: false, locale: 'fr-FR' });
  const page = await context.newPage();
  await page.goto('http://127.0.0.1:15175/preview/napstrfy.html');
  await expect(page.locator('meta[property="og:image"]')).toHaveAttribute('content', 'https://napstr.net/assets/napstrfy-share.png');
  await page.locator('.language-links summary').click();
  await expect(page.locator('.language-links a')).toHaveCount(10);
  await page.locator('.language-links a[lang="fr"]').click();
  await expect(page).toHaveURL(/\/preview\/fr\/napstrfy.html$/);
  await expect(page.locator('html')).toHaveAttribute('lang', 'fr');
  await context.close();
});

test('website downloads select the right product and display translated release status', async ({ page }) => {
  await page.route('https://api.github.com/**', (route) => route.fulfill({ contentType: 'application/json', body: JSON.stringify({ tag_name: 'v0.2.2', html_url: 'https://github.com/lnbits/napstr/releases/tag/v0.2.2', assets: ['Napstrfy_0.2.2_x64.exe', 'Napstr_0.2.2_x64.exe', 'Napstrfy_0.2.2_amd64.AppImage', 'Napstr_0.2.2_amd64.AppImage'].map((name) => ({ name, size: 1234567, browser_download_url: `https://github.com/lnbits/napstr/releases/download/v0.2.2/${name}` })) }) }));
  await page.goto('http://127.0.0.1:15175/es/download.html');
  await expect(page.locator('[data-release-platform="windows"]').first()).toHaveAttribute('href', /Napstr_0.2.2_x64.exe$/);
  await expect(page.locator('#release-status')).toContainText('0.2.2');
  await page.goto('http://127.0.0.1:15175/es/napstrfy.html');
  await expect(page.locator('[data-release-platform="windows"]')).toHaveAttribute('href', /Napstrfy_0.2.2_x64.exe$/);
});

test('Napstrfy downloads follow newly published assets and keep unavailable clients disabled', async ({ page }) => {
  let version = '0.2.1';
  let clients = ['android.apk'];
  await page.route('https://api.github.com/**', (route) => route.fulfill({ json: {
    tag_name: `v${version}`,
    html_url: `https://github.com/lnbits/napstr/releases/tag/v${version}`,
    assets: [...clients.map((client) => `Napstrfy_${version}_${client}`), `Napstr_${version}_x64.exe`].map((name) => ({
      name, size: 1234567, browser_download_url: `https://github.com/lnbits/napstr/releases/download/v${version}/${name}`
    }))
  } }));
  await page.goto('http://127.0.0.1:15175/napstrfy.html');
  await expect(page.locator('[data-release-version="windows"]')).toHaveText('Unavailable');
  await expect(page.locator('[data-release-platform="windows"]')).toHaveAttribute('aria-disabled', 'true');
  await expect(page.locator('[data-release-platform="windows"]')).not.toHaveAttribute('href');
  await expect(page.locator('[data-release-platform="napstrfy-android"]')).toHaveAttribute('href', /Napstrfy_0.2.1_android.apk$/);
  await expect(page.locator('#release-status')).toContainText('some installers are not available');
  version = '0.2.2';
  clients = ['android.apk', 'x64.exe', 'amd64.AppImage', 'aarch64.dmg', 'x64.dmg', 'ios.ipa'];
  await page.reload();
  await expect(page.locator('#release-status')).toHaveText('Napstrfy 0.2.2 downloads are ready.');
  for (const [platform, file] of Object.entries({ windows: 'x64.exe', linux: 'amd64.AppImage', 'macos-arm64': 'aarch64.dmg', 'macos-intel': 'x64.dmg', 'napstrfy-android': 'android.apk', 'napstrfy-ios': 'ios.ipa' })) {
    await expect(page.locator(`[data-release-platform="${platform}"]`)).toHaveAttribute('href', `https://github.com/lnbits/napstr/releases/download/v0.2.2/Napstrfy_0.2.2_${file}`);
    await expect(page.locator(`[data-release-version="${platform}"]`)).toHaveText('0.2.2');
  }
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: test.info().outputPath('napstrfy-downloads.png'), fullPage: true });
});

test('Napstrfy downloads retain a release-page fallback when GitHub is unavailable', async ({ page }) => {
  await page.route('https://api.github.com/**', (route) => route.fulfill({ status: 403, json: { message: 'API rate limit exceeded' } }));
  await page.goto('http://127.0.0.1:15175/napstrfy.html');
  await expect(page.locator('#release-status')).toHaveText('The automatic download list is temporarily unavailable.');
  await expect(page.locator('#release-page')).toHaveAttribute('href', 'https://github.com/lnbits/napstr/releases/latest');
  await expect(page.locator('[data-release-version="windows"]')).toHaveText('Unavailable');
  await expect(page.locator('[data-release-platform][href]')).toHaveCount(0);
});


test('switching language during playback preserves audio and updates Android media labels', async ({ page }) => {
  const wav = silentAudio();
  await mockNative(page);
  await page.addInitScript(() => {
    window.mediaUpdates = [];
    window.NapstrfyMedia = { update: (payload) => window.mediaUpdates.push(JSON.parse(payload)), clear: () => {} };
  });
  await page.route('**/fixture.wav', (route) => route.fulfill({ contentType: 'audio/wav', body: wav }));
  await page.goto('http://127.0.0.1:15174');
  await page.locator('.track-open').first().click();
  await expect.poll(() => page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
  const source = await page.locator('audio').getAttribute('src');
  await page.locator('.app-nav button').last().click();
  await page.locator('[data-language-select]').selectOption('fr');
  await expect.poll(() => page.evaluate(() => window.mediaUpdates.at(-1)?.labels.play)).toBe(catalogs.fr.Play);
  expect(await page.locator('audio').evaluate((audio) => audio.paused)).toBe(false);
  await expect(page.locator('audio')).toHaveAttribute('src', source);
  expect(await page.evaluate(() => window.calls.filter((call) => call.cmd === 'cache_remote_audio').length)).toBe(1);
});

test('Napstrfy labels unsigned iOS packages and prefers a signed IPA when available', async ({ page }) => {
  let names = ['Napstr_1.2.3_ios.ipa', 'Napstrfy_1.2.3_ios-unsigned.ipa'];
  await page.route('https://api.github.com/**', (route) => route.fulfill({ json: {
    tag_name: 'v1.2.3', assets: names.map((name) => ({ name, size: 1024,
      browser_download_url: `https://github.com/lnbits/napstr/releases/download/v1.2.3/${name}` }))
  } }));
  await page.goto('http://127.0.0.1:15175/napstrfy.html');
  const link = page.locator('[data-release-platform="napstrfy-ios"]');
  await expect(link).toHaveText('Download unsigned IPA');
  await expect(link).toHaveAttribute('href', /Napstrfy_1.2.3_ios-unsigned.ipa$/);
  await expect(page.getByText('IPA installation requires suitable signing and provisioning. This is not an App Store or TestFlight link.')).toBeVisible();
  names.push('Napstrfy_1.2.3_ios.ipa');
  await page.reload();
  await expect(link).toHaveText('Download IPA');
  await expect(link).toHaveAttribute('href', /Napstrfy_1.2.3_ios.ipa$/);
  names = ['Napstr_1.2.3_ios.ipa'];
  await page.reload();
  await expect(link).toHaveAttribute('aria-disabled', 'true');
  await expect(link).not.toHaveAttribute('href');
});
