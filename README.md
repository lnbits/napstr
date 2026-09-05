<img width="300" height="100" alt="napstr-logo-small" src="https://github.com/user-attachments/assets/83c9ccea-3241-4fce-a7cf-16c83629484b" />

<img height="400" alt="image" src="https://github.com/user-attachments/assets/95b4d9f8-a844-49e1-ba9a-6dd003c8acd3" />

Napstr uses Nostr for discovery and Tor for private file sharing.

https://napstr.net

The cross-platform Napstrfy phone companion lives in [`android/`](android/README.md).

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

macOS release DMGs are ad-hoc-signed community builds and require no Apple
Developer account. After the first blocked launch, open **System Settings →
Privacy & Security → Open Anyway**. Apple Silicon and Intel builds both include Tor.

## Implemented architecture

- On first launch, Napstr creates a Nostr identity and securely stores its private key using your operating system's credential store.
- Nostr publishes the searchable catalogue, live seeders, NIP-C7 trollbox, and per-track discussions; NIP-17 handles private download negotiation.
- A bundled Tor process carries transfers without a direct-IP fallback.
- The optional Napstrfy companion pairs by one-use QR and reaches the running desktop over encrypted Iroh.
- One recursively watched folder contains both downloads and shared audio.
- Napstr uses or creates a non-destructive `Audiobooks` drop zone: each child folder becomes an ordered book and each loose audio file becomes a one-file book. Existing contents are never replaced.
- Files are audio-validated and identified by SHA-256. Downloads use a responsive seeder, verify the complete hash, and are available in the built-in player.

Profiles and catalogue metadata are public. Requests, transfer credentials, file contents, and peer IP addresses are not published. Tor use may still be visible to an ISP.
