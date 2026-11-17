import test from 'node:test';
import assert from 'node:assert/strict';
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { execFileSync } from 'node:child_process';

const android = resolve('android');

test('Android configuration replaces template and stale launcher icons without removing other resources', async (t) => {
  const project = await mkdtemp(join(tmpdir(), 'napstrfy-android-resources-'));
  t.after(() => rm(project, { recursive: true, force: true }));
  const main = join(project, 'src-tauri/gen/android/app/src/main');
  const resources = join(main, 'res');
  const icons = join(project, 'src-tauri/icons/android');
  await cp(join(android, 'native'), join(project, 'native'), { recursive: true });
  await cp(join(android, 'src-tauri/icons/android'), icons, { recursive: true });
  await mkdir(join(resources, 'values'), { recursive: true });
  await writeFile(join(resources, 'values/strings.xml'), '<resources><string name="app_name">Napstrfy</string></resources>');
  await writeFile(join(main, 'AndroidManifest.xml'), `<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <uses-permission android:name="android.permission.INTERNET" />
    <application android:icon="@mipmap/ic_launcher">
        <provider android:name="androidx.core.content.FileProvider" />
    </application>
</manifest>`);
  await writeFile(join(main, '../../build.gradle.kts'), 'dependencies {\n}\n');
  const files = (await readdir(icons, { recursive: true })).filter((file) => /\.(png|xml)$/.test(file));
  assert.equal(files.length, 17);
  for (let run = 0; run < 2; run++) {
    // Simulate both a fresh Tauri template and an existing project with stale assets.
    for (const file of files) {
      await mkdir(resolve(resources, file, '..'), { recursive: true });
      await writeFile(join(resources, file), 'stale template icon');
    }
    execFileSync(process.execPath, [join(android, 'scripts/configure-android.mjs')], {
      cwd: project, env: { ...process.env, NAPSTRFY_ANDROID_RELEASE_SIGNING: '0' }
    });
    for (const file of files) {
      assert.deepEqual(await readFile(join(resources, file)), await readFile(join(icons, file)), file);
    }
    assert.match(await readFile(join(resources, 'values/strings.xml'), 'utf8'), /Napstrfy/);
    assert.match(await readFile(join(resources, 'mipmap-anydpi-v26/ic_launcher.xml'), 'utf8'), /@mipmap\/ic_launcher_foreground/);
    assert.match(await readFile(join(main, 'AndroidManifest.xml'), 'utf8'), /android:icon="@mipmap\/ic_launcher"/);
  }
});
