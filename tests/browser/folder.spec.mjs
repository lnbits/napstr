import { test, expect } from '@playwright/test';
import { mockNative } from './helpers/native.mjs';

async function setupFolderPicker(page, outcome) {
  await mockNative(page, { app: 'napstr' });
  await page.addInitScript((outcome) => {
    const native = window.__TAURI_INTERNALS__;
    const invoke = native.invoke;
    const callbacks = new Map();
    const listeners = new Map();
    let nextId = 1;
    let folder = '/music';
    native.transformCallback = (callback) => {
      const id = nextId++;
      callbacks.set(id, callback);
      return id;
    };
    native.invoke = async (cmd, args = {}) => {
      if (cmd === 'plugin:event|listen') listeners.set(args.event, args.handler);
      if (cmd === 'plugin:dialog|open') return outcome === 'cancel' ? null : '/new-music';
      if (cmd === 'set_napstr_folder') {
        window.calls.push({ cmd, args });
        if (outcome === 'rejected') throw 'The selected folder is not writable';
        folder = args.path;
        if (outcome === 'scan-failed') throw 'database is locked';
        if (outcome === 'pending') {
          callbacks.get(listeners.get('napstr-library-changed'))({ payload: null });
          await new Promise((resolve) => { window.finishFolderScan = resolve; });
        }
        return { fileCount: 0, totalBytes: 0, errors: [], errorCount: 0, changedFiles: 0 };
      }
      const response = await invoke(cmd, args);
      if (cmd === 'get_snapshot' || cmd === 'save_settings') {
        // Model a backend which would accept stale settings: the UI must send
        // its refreshed path when Apply is clicked, even while scanning.
        if (cmd === 'save_settings') folder = args.settings.napstrFolder;
        response.settings.napstrFolder = folder;
      }
      return response;
    };
  }, outcome);
  await page.goto('http://127.0.0.1:15173');
  await page.locator('.tool-button').filter({ hasText: /Shared$/ }).click();
  await expect(page.locator('.folder-path input')).toHaveValue('/music');
}

for (const outcome of ['success', 'pending', 'scan-failed', 'cancel', 'rejected']) {
  test(`folder selection: ${outcome} displays the saved path and preserves it through Apply`, async ({ page }) => {
    await setupFolderPicker(page, outcome);
    await page.locator('.folder-path').getByRole('button', { name: 'Browse…', exact: true }).click();
    const expected = ['cancel', 'rejected'].includes(outcome) ? '/music' : '/new-music';
    if (outcome === 'scan-failed') await expect(page.locator('.network-strip')).toContainText('database is locked');
    if (outcome === 'rejected') await expect(page.locator('.network-strip')).toContainText('not writable');
    await expect(page.locator('.folder-path input')).toHaveValue(expected);
    await page.locator('.tool-button').filter({ hasText: /Settings$/ }).click();
    await expect(page.locator('fieldset').filter({ hasText: 'Downloads and shared audio' }).locator('input[readonly]')).toHaveValue(expected);
    await page.getByRole('button', { name: 'Apply', exact: true }).click();
    await expect.poll(() => page.evaluate(() => window.calls.filter(({ cmd }) => cmd === 'save_settings').at(-1)?.args.settings.napstrFolder)).toBe(expected);
    if (outcome === 'pending') await page.evaluate(() => window.finishFolderScan());
    await page.locator('.tool-button').filter({ hasText: /Shared$/ }).click();
    await expect(page.locator('.folder-path input')).toHaveValue(expected);
  });
}
