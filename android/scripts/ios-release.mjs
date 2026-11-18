import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { PNG } from 'pngjs';

const root = fileURLToPath(new URL('../', import.meta.url));

export function iosVersion(version, buildNumber) {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$/.exec(version);
  if (!match) throw new Error('Invalid iOS release version');
  if (!/^[1-9]\d{0,8}$/.test(String(buildNumber))) throw new Error('Invalid iOS build number');
  return { version: match.slice(1, 4).join('.'), bundle: { iOS: { bundleVersion: String(buildNumber) } } };
}

export function prepareProject(project) {
  const assets = '      - path: Assets.xcassets\n';
  if (!project.includes(assets)) throw new Error('Unexpected iOS project: missing asset source');
  if (!project.includes('      - path: PrivacyInfo.xcprivacy\n')) {
    project = project.replace(assets, assets + '      - path: PrivacyInfo.xcprivacy\n        buildPhase: resources\n');
  }
  project = project.replace(/      - path: Externals\n(?!        excludes:)/,
    '      - path: Externals\n        excludes: ["**/*.a"]\n');
  const framework = '      - sdk: CoreGraphics.framework\n';
  if (!project.includes(framework)) throw new Error('Unexpected iOS project: missing framework list');
  for (const sdk of ['AVFoundation.framework', 'Network.framework', 'SystemConfiguration.framework', 'libresolv.tbd']) {
    if (!project.includes(`      - sdk: ${sdk}\n`)) project = project.replace(framework, framework + `      - sdk: ${sdk}\n`);
  }
  return project;
}

export function opaqueIcon(input) {
  const image = PNG.sync.read(input);
  // Tauri composites on the supplied background; image-rs can round alpha to 254.
  for (let offset = 3; offset < image.data.length; offset += 4) {
    if (image.data[offset] < 254) throw new Error('iOS icon contains transparency');
    image.data[offset] = 255;
  }
  return PNG.sync.write(image, { colorType: 2 });
}

function configureVersion() {
  const version = process.env.RELEASE_TAG
    ? process.env.RELEASE_TAG.replace(/^v/, '')
    : JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).version;
  const config = iosVersion(version, process.env.GITHUB_RUN_NUMBER || '1');
  writeFileSync(join(root, 'src-tauri/tauri.ios.ci.conf.json'), JSON.stringify(config) + '\n');
  return version;
}

function prepare() {
  const apple = join(root, 'src-tauri/gen/apple');
  const projectFile = join(apple, 'project.yml');
  writeFileSync(projectFile, prepareProject(readFileSync(projectFile, 'utf8')));
  copyFileSync(join(root, 'src-tauri/mobile/ios/PrivacyInfo.xcprivacy'), join(apple, 'PrivacyInfo.xcprivacy'));
  const temporary = mkdtempSync(join(tmpdir(), 'napstrfy-ios-icons-'));
  try {
    execFileSync('npm', ['run', 'tauri', '--', 'icon', '../src-tauri/icons/icon.png', '--output', temporary, '--ios-color', '#fff'], { cwd: root, stdio: 'pipe' });
    const icons = join(apple, 'Assets.xcassets/AppIcon.appiconset');
    const manifest = JSON.parse(readFileSync(join(icons, 'Contents.json'), 'utf8'));
    for (const filename of new Set(manifest.images.map((entry) => entry.filename))) {
      if (!filename || !/^AppIcon-[\w.@-]+\.png$/.test(filename)) throw new Error('Unexpected iOS icon filename');
      writeFileSync(join(icons, filename), opaqueIcon(readFileSync(join(temporary, 'ios', filename))));
    }
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
  execFileSync('xcodegen', ['generate'], { cwd: apple, stdio: 'inherit' });
  // XcodeGen regenerates Info.plist. Reapply the app's camera/local-network text.
  const plists = readdirSync(apple).filter((name) => name.endsWith('_iOS'))
    .map((name) => join(apple, name, 'Info.plist')).filter(existsSync);
  if (plists.length !== 1) throw new Error('Expected one generated iOS Info.plist');
  const [plist] = plists;
  const readPlist = (path) => JSON.parse(execFileSync('plutil', ['-convert', 'json', '-o', '-', path], { encoding: 'utf8' }));
  execFileSync('plutil', ['-convert', 'xml1', '-o', plist, '-'], {
    input: JSON.stringify({ ...readPlist(plist), ...readPlist(join(root, 'src-tauri/Info.ios.plist')) })
  });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === 'version') configureVersion();
  else if (process.argv[2] === 'prepare') prepare();
  else throw new Error('Usage: ios-release.mjs version|prepare');
}
