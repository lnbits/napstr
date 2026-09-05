import { copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const main = resolve('src-tauri/gen/android/app/src/main');
const native = resolve('native/android');
const manifestPath = resolve(main, 'AndroidManifest.xml');
let manifest = await readFile(manifestPath, 'utf8');

const internetPermission = '    <uses-permission android:name="android.permission.INTERNET" />';
const mediaPermissions = [
  '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE" />',
  '    <uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK" />',
  '    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />',
  '    <uses-permission android:name="android.permission.WAKE_LOCK" />',
].filter((permission) => !manifest.includes(permission.trim()));
if (mediaPermissions.length > 0) {
  manifest = manifest.replace(
    internetPermission,
    `${internetPermission}\n${mediaPermissions.join('\n')}`,
  );
}

const application = '<application';
if (manifest.includes('android:allowBackup=')) {
  manifest = manifest.replace(/android:allowBackup="[^"]*"/, 'android:allowBackup="false"');
} else {
  manifest = manifest.replace(application, `${application}\n        android:allowBackup="false"`);
}
if (!manifest.includes('android:dataExtractionRules=')) {
  manifest = manifest.replace(
    application,
    `${application}\n        android:dataExtractionRules="@xml/data_extraction_rules"`,
  );
}
if (!manifest.includes('android:fullBackupContent=')) {
  manifest = manifest.replace(
    application,
    `${application}\n        android:fullBackupContent="@xml/backup_rules"`,
  );
}
if (!manifest.includes('android:networkSecurityConfig=')) {
  manifest = manifest.replace(
    application,
    `${application}\n        android:networkSecurityConfig="@xml/network_security_config"`,
  );
}
if (!manifest.includes('android:name=".MediaNotificationService"')) {
  manifest = manifest.replace(
    '        <provider',
    `        <service
            android:name=".MediaNotificationService"
            android:exported="false"
            android:foregroundServiceType="mediaPlayback" />

        <provider`,
  );
}
await writeFile(manifestPath, manifest);

const javaDirectory = resolve(main, 'java/net/napstr/nostrfy');
await mkdir(javaDirectory, { recursive: true });
for (const filename of [
  'MainActivity.kt',
  'MediaControlBridge.kt',
  'MediaNotificationService.kt',
]) {
  await copyFile(resolve(native, filename), resolve(javaDirectory, filename));
}

const xmlDirectory = resolve(main, 'res/xml');
await mkdir(xmlDirectory, { recursive: true });
await writeFile(
  resolve(xmlDirectory, 'network_security_config.xml'),
  `<?xml version="1.0" encoding="utf-8"?>
<network-security-config>
    <base-config cleartextTrafficPermitted="false" />
    <domain-config cleartextTrafficPermitted="true">
        <domain includeSubdomains="false">127.0.0.1</domain>
    </domain-config>
</network-security-config>
`,
);
const drawableDirectory = resolve(main, 'res/drawable');
await mkdir(drawableDirectory, { recursive: true });
await copyFile(
  resolve(native, 'ic_stat_napstrfy.xml'),
  resolve(drawableDirectory, 'ic_stat_napstrfy.xml'),
);
await writeFile(
  resolve(xmlDirectory, 'backup_rules.xml'),
  `<?xml version="1.0" encoding="utf-8"?>
<full-backup-content>
    <exclude domain="root" path="." />
    <exclude domain="file" path="." />
    <exclude domain="database" path="." />
    <exclude domain="sharedpref" path="." />
    <exclude domain="external" path="." />
</full-backup-content>
  `,
);

const gradlePath = resolve('src-tauri/gen/android/app/build.gradle.kts');
let gradle = await readFile(gradlePath, 'utf8');
if (!gradle.includes('androidx.media:media:')) {
  gradle = gradle.replace(
    'dependencies {',
    'dependencies {\n    implementation("androidx.media:media:1.7.1")',
  );
}

if (process.env.NAPSTRFY_ANDROID_RELEASE_SIGNING === '1') {

  if (!gradle.includes('import java.io.FileInputStream')) {
    gradle = gradle.replace(
      'import java.util.Properties',
      'import java.io.FileInputStream\nimport java.util.Properties',
    );
  }

  if (!gradle.includes('create("release")')) {
    gradle = gradle.replace(
      '    buildTypes {',
      `    signingConfigs {
        create("release") {
            val keystorePropertiesFile = rootProject.file("keystore.properties")
            require(keystorePropertiesFile.exists()) {
                "Release signing requires gen/android/keystore.properties"
            }
            val keystoreProperties = Properties().apply {
                load(FileInputStream(keystorePropertiesFile))
            }
            keyAlias = keystoreProperties["keyAlias"] as String
            keyPassword = keystoreProperties["password"] as String
            storeFile = file(keystoreProperties["storeFile"] as String)
            storePassword = keystoreProperties["password"] as String
        }
    }
    buildTypes {`,
    );
  }

  if (!gradle.includes('signingConfig = signingConfigs.getByName("release")')) {
    gradle = gradle.replace(
      '        getByName("release") {',
      '        getByName("release") {\n            signingConfig = signingConfigs.getByName("release")',
    );
  }
}
await writeFile(gradlePath, gradle);
await writeFile(
  resolve(xmlDirectory, 'data_extraction_rules.xml'),
  `<?xml version="1.0" encoding="utf-8"?>
<data-extraction-rules>
    <cloud-backup>
        <exclude domain="root" path="." />
        <exclude domain="file" path="." />
        <exclude domain="database" path="." />
        <exclude domain="sharedpref" path="." />
        <exclude domain="external" path="." />
    </cloud-backup>
    <device-transfer>
        <exclude domain="root" path="." />
        <exclude domain="file" path="." />
        <exclude domain="database" path="." />
        <exclude domain="sharedpref" path="." />
        <exclude domain="external" path="." />
    </device-transfer>
</data-extraction-rules>
`,
);
