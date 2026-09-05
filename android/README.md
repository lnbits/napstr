# Napstrfy

Napstrfy is the Android companion for a running Napstr desktop.

## Requirements

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

Pair the phone from Napstr's **Mobile** page. Napstr must remain running while Napstrfy is in use.
Podcasts are independent of Napstr: Napstrfy searches a public podcast directory and streams or downloads episodes directly from their publishers.

The same codebase can later be built for iOS from a Mac using `npm run ios:init` and `npm run ios:dev`.
