# 🧅 Napstr for Umbrel & Community App Store

[![Umbrel OS](https://img.shields.io/badge/Umbrel-Community%20App-00ff66?style=for-the-badge&logo=docker)](https://umbrel.com)
[![Architecture](https://img.shields.io/badge/Arch-amd64%20%7C%20arm64-blue?style=for-the-badge)](https://github.com/borocode/napstr-umbrel)
[![License](https://img.shields.io/badge/License-MIT-black?style=for-the-badge)](LICENSE)

Run a sovereign, self-hosted **P2P music library & streaming node** directly on your **Umbrel server** (Raspberry Pi 4/5 or x86_64 Home Server). Napstr uses **Nostr** for discovery and **Tor** for private, lossless file transfer — no central server, no IP leaks, your catalogue published to the decentralized swarm. This repository is an **Umbrel community port** of [`lnbits/napstr`](https://github.com/lnbits/napstr), packaged and CI-hardened by [Boro Labs](https://github.com/borocode).

<img height="400" alt="napstr-ui" src="https://github.com/user-attachments/assets/95b4d9f8-a844-49e1-ba9a-6dd003c8acd3" />

The cross-platform Napstrfy phone companion lives in [`android/`](android/README.md).

---

> [!NOTE]
> **Umbrel Release v0.2.0:** This package is built directly against upstream `lnbits/napstr` v0.2.0 with full multi-arch containerization (`linux/amd64` and `linux/arm64` for Raspberry Pi 4/5) and the headless Umbrel server daemon.

---

## 🌟 Features

- **1-Click Umbrel Deployment:** Pre-configured Docker orchestration (`docker-compose.yml` + `umbrel-app.yml`) with persistent state at `/data` and your music library mounted at `/music`.
- **Sovereign P2P Stack:** Nostr (Kind 30421 catalogues, Kind 30422 availability heartbeats, NIP-17 private negotiation) for discovery + Tor v3 onion services for transfer — no direct-IP fallback, no central server.
- **Web UI + Trollbox:** Built-in Svelte player and live NIP-C7 chat, exposed via Umbrel's app proxy on port `30421`.
- **Headless Daemon:** Runs as a server binary (no desktop/GUI session needed) — the right shape for a 24/7 Pi.
- **Multi-Architecture Support:** Native multi-stage builds for both `linux/amd64` (PC / Intel / AMD) and `linux/arm64` (Raspberry Pi 4/5), published to GHCR via hosted CI.

---

## 📦 Umbrel App Store Installation

### Method 1: Add as a Community App Store (Instant 1-Click)

1. Open your **Umbrel Dashboard** (e.g. `http://umbrel.local`).
2. Navigate to **App Store** → Click the **Three Dots (⋮)** in the top right → **Community App Stores**.
3. Paste this repository URL:
   ```text
   https://github.com/borocode/napstr-umbrel
   ```
4. Click **Add**. **Napstr** will appear in your App Store ready to install!

### Method 2: Manual Local Installation via SSH

```bash
# 1. SSH into your Umbrel server
ssh umbrel@10.0.0.67

# 2. Create the app-data directory
mkdir -p ~/umbrel/app-data/napstr

# 3. Clone this repository
git clone https://github.com/borocode/napstr-umbrel.git temp
cp -r temp/* ~/umbrel/app-data/napstr/
rm -rf temp

# 4. Start the app via Docker Compose
cd ~/umbrel/app-data/napstr
docker compose up -d
```

---

## 🔧 What This Umbrel Port Changes (vs. upstream `lnbits/napstr`)

This is a **packaging fork**, not a feature fork. The daemon, web UI, Nostr/Tor logic, and protocol are upstream's. We changed only what's needed to run it as a headless, multi-arch Umbrel app.

### ➕ Additions

- **`umbrel-app.yml`** — Umbrel community-store manifest (id `napstr`, port `30421`, Media category, Tor/Nostr tagline).
- **`docker-compose.yml`** — three services: `server` (Napstr daemon + web UI), `app_proxy` (Umbrel reverse proxy), and `tor_server` (bundled Tor sidecar). Mounts `/data` (state) and `/music` (your library).
- **`Dockerfile.umbrel`** — multi-stage build: Svelte frontend → Rust `napstr-daemon` → minimal Debian runtime. Publishes a multi-arch image to GHCR.
- **Full GTK/WebKit runtime stack in the runtime image** — `libwebkit2gtk-4.1-0`, `libgtk-3-0`, `libsoup-3.0-0`, `libjavascriptcoregtk-4.1-0`, `libdbus-1-3`, `libasound2`. The `src-tauri` daemon links the WebKitGTK stack **even in headless mode**, so these are required at runtime or the container crash-loops on `error while loading shared libraries`.
- **Headless key-persistence fallback** — the daemon stores its Nostr identity in the OS credential store on desktop; in a container we fall back to a file under `DATA_DIR` so the key survives restarts.
- **Hosted CI** (`.github/workflows/docker-publish.yml`) — multi-arch build on `ubuntu-latest` (the self-hosted Windows runner couldn't do multi-platform builds).

### ➖ Removals / Divergences

- **No desktop/Tauri app** — upstream ships a `npm run desktop` Tauri GUI; this port builds and runs only the `napstr-daemon` server binary. The ~250MB of GUI-only libraries remain in the runtime image only because the daemon still links them (a future Option-B refactor would decouple and shed them).
- **No `npm run bundle` / bundled-Tor-download step** — Tor is installed via `apt` in the runtime image and run as a separate `tor_server` container, not downloaded at build time.
- **App version aligned with upstream release `0.2.0`** in `umbrel-app.yml`.

---

## 🌐 Network & Port Allocations

| Port | Protocol | Purpose |
| :--- | :--- | :--- |
| **`30421`** | `HTTP` | Napstr Web UI, API & daemon (via Umbrel app proxy) |
| **`9050`** | `TCP` | Tor SOCKS proxy (internal, `tor_server`) |

Music is served losslessly over ephemeral Tor v3 onion services; the web UI is reachable on your LAN through Umbrel's reverse proxy.

---

## 🛠️ Upstream Submission / Sync

To stay current with [`lnbits/napstr`](https://github.com/lnbits/napstr):

1. `git remote add upstream https://github.com/lnbits/napstr.git`
2. `git fetch upstream && git merge upstream/main`
3. Re-apply the Umbrel additions above (they live in `umbrel-app.yml`, `docker-compose.yml`, `Dockerfile.umbrel`).
4. Confirm the runtime-image WebKit/GTK libs still cover whatever `src-tauri` now links, then let CI rebuild.

For official [`getumbrel/umbrel-apps`](https://github.com/getumbrel/umbrel-apps) submission: copy the `napstr/` package directory into your fork and open a PR — same path as `octra-umbrel`.

## 🏗️ Architecture & Core Protocols

- **Identity & Keys:** On first launch, Napstr creates a Nostr identity and securely persists keys for headless container reboots.
- **Discovery & Swarm:** Nostr relays publish searchable Kind 30421 catalogues, Kind 30422 live seeder heartbeats, NIP-C7 trollbox, and per-track discussions; NIP-17 handles private download negotiation.
- **Lossless Tor Transfers:** Ephemeral Tor v3 onion services carry all audio transfers without direct-IP leaks or central servers.
- **Audiobooks & Napstrfy:** Built-in support for `Audiobooks` drop zones and optional pairing with the Napstrfy mobile companion via encrypted Iroh.
- **Integrity:** Lossless audio streaming with SHA-256 chunk validation.

---

## 📜 License

MIT License. Core software developed by [`lnbits/napstr` contributors](https://github.com/lnbits/napstr). Umbrel packaging and daemon integration maintained by [Boro Labs](https://github.com/borocode).
