import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { iosVersion, prepareProject, opaqueIcon } from '../android/scripts/ios-release.mjs';
const { PNG } = createRequire(new URL('../android/package.json', import.meta.url))('pngjs');

test('iOS marketing versions omit prerelease suffixes and retain a separate numeric build', () => {
  assert.deepEqual(iosVersion('1.2.3-rc1', '42'), { version: '1.2.3', bundle: { iOS: { bundleVersion: '42' } } });
  for (const version of ['bad', 'v1.2.3', '01.2.3', '1.2', '1.2.3\n']) assert.throws(() => iosVersion(version, 1));
  for (const build of ['0', '-1', '1.2', 'NaN', '1\n']) assert.throws(() => iosVersion('1.2.3', build));
});

test('iOS project preparation bundles privacy metadata and frameworks without embedding static libraries', () => {
  const input = '      - path: Assets.xcassets\n      - path: Externals\n      - sdk: CoreGraphics.framework\n';
  const output = prepareProject(input);
  assert.match(output, /PrivacyInfo.xcprivacy\n        buildPhase: resources/);
  assert.match(output, /excludes: \["\*\*\/\*.a"\]/);
  for (const framework of ['AVFoundation.framework', 'Network.framework', 'SystemConfiguration.framework', 'libresolv.tbd']) assert.ok(output.includes(framework));
  assert.equal(prepareProject(output), output);
  assert.throws(() => prepareProject('unknown template'));
});

test('iOS icons retain RGB pixels, remove rounding alpha and reject real transparency', () => {
  const png = new PNG({ width: 1, height: 1 });
  png.data.set([10, 20, 30, 254]);
  const output = opaqueIcon(PNG.sync.write(png));
  assert.equal(output[25], 2); // RGB, with no alpha channel.
  assert.deepEqual([...PNG.sync.read(output).data], [10, 20, 30, 255]);
  png.data[3] = 0;
  assert.throws(() => opaqueIcon(PNG.sync.write(png)), /transparency/);
});
