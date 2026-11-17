<img width="300" height="100" alt="napstr-logo-small" src="https://github.com/user-attachments/assets/83c9ccea-3241-4fce-a7cf-16c83629484b" />

<img height="400" alt="image" src="https://github.com/user-attachments/assets/95b4d9f8-a844-49e1-ba9a-6dd003c8acd3" />

Napstr uses Nostr for discovery and Tor for private file sharing.

https://napstr.net

The cross-platform Napstrfy phone companion lives in [`android/`](android/README.md).

For an always-on Linux VPS seeder, see the separate [Napstr daemon](daemon/README.md).
Release builds include standalone x86_64 and ARM64 daemon archives.

## Build your own Napstr!

See the complete [Napstr protocol specification](PROTOCOL.md) for everything
needed to build an interoperable client.

## Build from source

Install [Node.js](https://nodejs.org/), [Rust](https://rustup.rs/), and the
[Tauri prerequisites for your OS](https://v2.tauri.app/start/prerequisites/),
then:

```bash
git clone https://github.com/lnbits/napstr.git
cd napstr
npm ci
npm run desktop
```

Development builds use `NAPSTR_TOR_PATH` when set, then a bundled Tor binary, then `tor` on `PATH`.

On macOS, install Tor with Homebrew and start Napstr with:

```bash
brew install tor
NAPSTR_TOR_PATH="$(command -v tor)" npm run desktop
```

To create a package for your operating system with Tor included, run:

```bash
npm run bundle
```

Napstr automatically downloads and verifies the pinned official Tor Expert
Bundle for your platform before building.

On macOS, build a verified DMG and SHA-256 checksum with:

```bash
npm run macos-build         # Ad-hoc community build; no Apple credentials
npm run macos-build:signed  # Developer ID signed, notarized, and stapled
```

Both commands build for the current Mac's native architecture and include Tor.
Signed builds read the private root `.env.macos-release` file. Neither command
publishes a release. See [macOS release setup](docs/macos-releases.md) for
credentials, output paths, verification, and the equivalent Napstrfy commands.

To request Napstr macOS installers from GitHub Actions, prefix your PR title with
`[build]`, for example `[build] Fix audio playback`. Adding the prefix to an
existing PR also starts a build; new commits rebuild while the prefix is present.
Same-repository PRs produce signed installers using the Apple signing secrets.
Fork and Dependabot PRs produce artifacts ending in `-unsigned`.
These builds use the existing **Build Napstr desktop and Napstrfy Android
installers** workflow (`release.yml`). For a manual macOS test build, run that
workflow on your branch with `release_tag` empty; providing a tag runs the full
release process.
Download `Napstr-macOS-Intel-signed` or `Napstr-macOS-Apple-Silicon-signed` from
the completed signed run to test on your Mac.

GitHub release DMGs are signed and notarized for Apple Silicon and Intel.
Local community builds still require **System Settings → Privacy & Security →
Open Anyway** after the first blocked launch.

## Implemented architecture

- On first launch, Napstr creates a Nostr identity and securely stores its private key using your operating system's credential store.
- Nostr publishes the searchable catalogue, live seeders, NIP-C7 trollbox, and per-track discussions; NIP-17 handles private download negotiation.
- Click a user name in seeder lists, profiles, or chat to browse that user's currently shared tracks in Search. Clear the name and search again to return to normal song searches. Exact names already discovered by Napstr and full public keys (`npub` or hex) can also be searched; duplicate names offer a choice of public keys.
- A bundled Tor process carries transfers without a direct-IP fallback.
- The optional Napstrfy companion pairs by one-use QR and reaches the running desktop over encrypted Iroh.
- The Napstrfy page has separate full-access and read-only QR codes. Read-only phones can play and cache the local catalogue for offline listening, but cannot request downloads on the host; see [pairing details](android/README.md#build-and-install-a-debug-apk).
- One recursively watched folder contains both downloads and shared audio.
- Napstr uses or creates a non-destructive `Audiobooks` drop zone: each child folder becomes an ordered book and each loose audio file becomes a one-file book. Existing contents are never replaced.
- Files are audio-validated and identified by SHA-256. Downloads use a responsive seeder, verify the complete hash, and are available in the built-in player.

Profiles and catalogue metadata are public. Requests, transfer credentials, file contents, and peer IP addresses are not published. Tor use may still be visible to an ISP.
