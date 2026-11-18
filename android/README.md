# Napstrfy

Napstr's companion player for Android, iOS, Windows, Linux, and macOS.

For desktop pairing, paste the code from Napstr's **Napstrfy → Pair without a camera**.

## Desktop

From `android/`, with the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) installed:

```sh
npm ci
npm run desktop          # Develop
npm run bundle:windows   # EXE (on Windows)
npm run macos-build      # Ad-hoc community DMG (on macOS)
npm run macos-build:signed # Signed, notarized, and stapled DMG (on macOS)
npm run appimage:build   # AppImage (Linux with Docker)
```

On NixOS, run from the repository root:

```sh
nix develop --command npm --prefix android run desktop
```

The development shell includes the GStreamer plugins WebKit needs for audio
playback. After changing the shell dependencies, stop the running client and
run this command again to load the updated environment.

Pushing a `v*` tag attaches installers and SHA-256 checksum files to a draft release.
Release macOS builds require Apple signing credentials; pull-request builds use
ad-hoc signatures. `bundle:macos` remains an alias for `macos-build`.
Both macOS commands build the current Mac's native architecture, share the root
`.env.macos-release` for signed local builds, and never upload anything. See
[macOS release setup](../docs/macos-releases.md).

## Android requirements

- Node.js
- Rust and Cargo
- JDK 17
- Android SDK, platform tools and build tools
- Android NDK
- `adb`

Install the Rust Android targets:

```sh
rustup default stable
rustup target add \
  aarch64-linux-android \
  armv7-linux-androideabi \
  i686-linux-android \
  x86_64-linux-android
```

## First setup

```sh
cd android
npm ci
npm run android:init
```

Enable USB debugging on your phone, connect it and check that it is available:

```sh
adb devices
```

## Run on a phone

Napstrfy keeps the screen awake while the app is open in the foreground. The power
button still locks the phone, and the normal screen timeout applies after leaving
the app.

For a phone connected over USB, use:

```sh
npm run android:dev:usb
```

This tunnels the development server through `adb`, so the phone does not need to reach the computer over Wi-Fi.

For wireless development, use:

```sh
npm run android:dev
```

## Build and install a debug APK

```sh
npm run android:build -- --debug --apk
adb install -r src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
```

Pair the phone from Napstr's **Napstrfy** page. Napstr must remain running to browse or fetch its audio; songs already cached on the phone can play offline.

The Napstrfy page offers two independent, one-use QR codes:

- **Full access** keeps the usual library search, Tor download requests, and verified offline audio cache.
- **Read only** lets a phone browse, play, and cache music and audiobooks already on this Napstr. Playback uses the same verified offline audio cache, seeking, and next-track prefetching as full access. Napstr rejects requests to download new songs on the host and access its transfer history. Update both apps to get this playback behaviour.

Each code expires after five minutes. Generating a replacement affects only that code's access mode. Each paired phone shows its access level; pairing the same phone again changes its access. Removing a phone rejects subsequent requests and stops active audio transfers at the next chunk check. Audio already cached on the phone remains available offline. Read-only access protects the host from download requests; it does not restrict copying audio.

Podcasts are independent of Napstr: Napstrfy searches a public podcast directory and streams or downloads episodes directly from their publishers.

## iOS packages

The **Build Napstrfy iOS package** GitHub Actions workflow runs on `v*` tags,
manual dispatches, and relevant pull requests. It produces a device IPA and a
SHA-256 checksum in the **Napstrfy-iOS** Actions artifact. Pull requests use
unsigned packages and receive no signing credentials. Successful release builds
also attach the IPA and checksum to the matching GitHub release, creating a draft
if it does not exist. This applies to tag builds and release workflow dispatches
with a release tag. A standalone manual iOS build on a branch produces Actions
artifacts only. The workflow does not publish drafts or submit to App Store
Connect/TestFlight.

Like Anagram, builds without iOS signing credentials produce
`Napstrfy_<version>_ios-unsigned.ipa`. This needs signing before installation on
an iPhone. Partial signing configuration fails rather than silently falling back.
Signed builds use the same secret names as Anagram:

- `APPLE_TEAM_ID` (the existing team can be reused)
- `IOS_CERTIFICATE`: base64 iOS signing certificate/private key in a `.p12`
- `IOS_CERTIFICATE_PASSWORD`: the `.p12` export password
- `IOS_MOBILE_PROVISION`: base64 profile covering `net.napstr.nostrfy`

The macOS Developer ID certificate and Anagram-specific provisioning profile
cannot substitute for Napstrfy's iOS credentials. The build validates the profile
against the bundle ID, team, expiry and export method without logging its contents.
`IOS_EXPORT_METHOD` defaults to `app-store-connect`; use `release-testing` with an
ad hoc profile for registered devices, or `debugging` with development credentials.
An App Store export is for subsequent App Store Connect/TestFlight distribution,
not direct installation from a website.

After the release containing the IPA and checksum is published, the website
enables its iOS download. It labels unsigned IPAs and prefers a signed
IPA when both are available. Until then the download stays unavailable.

Local builds require macOS, Xcode with the iOS SDK, and XcodeGen:

```sh
cd android
npm ci
rustup target add aarch64-apple-ios
npm run ios:init
npm run ios:dev                    # Xcode/device development
npm run ios:package -- --unsigned # Unsigned device IPA
npm run ios:package                # Signed when credentials are configured
```

Set `APPLE_DEVELOPMENT_TEAM` locally to the Apple team ID for signing. CI maps
`APPLE_TEAM_ID` to that environment variable. Marketing versions omit prerelease
suffixes; artifact filenames retain them, and the Actions run number becomes the
iOS build number. Local builds default to build 1; set `GITHUB_RUN_NUMBER` to a
higher number when exporting another build of the same version.

Generated Xcode files are disposable. The preparation step installs Napstrfy's
opaque icons, camera/local-network permission descriptions, privacy manifest and
framework links. See `src-tauri/vendor/README.md` for the narrow iOS dependency
patches adapted from Anagram. A successful Linux test run is not a device smoke test;
check pairing, QR scanning and audio playback on an iPhone before publishing.
