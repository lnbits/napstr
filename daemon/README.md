# Napstr VPS daemon

`napstrd` is a separate, foreground Linux music seeder. It recursively indexes
your music folder, publishes Napstr catalogue and availability events on Nostr,
and answers encrypted download requests with private Tor transfers. It needs
neither a desktop session nor an audio device. The desktop and phone builds are
unchanged. Napstrfy can pair over Iroh to browse, play, and cache music and
audiobooks.
Full-access devices can also search Nostr and request verified Tor downloads
onto the VPS. Files under `Audiobooks` are grouped for the companion using the
same folder and chapter-order conventions as the desktop. Public Nostr
publication still shares those chapters as individual files.

## Install on a VPS

Download the `napstrd_<version>_linux_x86_64.tar.gz` archive (or `aarch64` for an
ARM64 VPS) and its `.sha256` file from the Napstr GitHub release. The executable
is statically linked; install Tor separately using your distribution's packages.
For Debian/Ubuntu, from the directory containing your downloaded files:

```bash
sha256sum --check napstrd_*.tar.gz.sha256
tar -xzf napstrd_<version>_linux_x86_64.tar.gz
cd napstrd_<version>_linux_x86_64
sudo apt-get update
sudo apt-get install -y tor ca-certificates
sudo useradd --system --user-group --home-dir /var/lib/napstrd --shell /usr/sbin/nologin napstrd
sudo install -m 755 napstrd /usr/local/bin/napstrd
sudo install -o napstrd -g napstrd -m 700 -d /var/lib/napstrd
sudo install -o napstrd -g napstrd -m 750 -d /srv/napstr/music
sudo install -o root -g root -m 600 napstrd.env.example /etc/napstrd.env
sudo install -m 644 napstrd.service /etc/systemd/system/napstrd.service
sudoedit /etc/napstrd.env
```

Replace `NAPSTR_NSEC` with your nsec and set `NAPSTR_MUSIC_DIR` to the absolute
path of your music folder. Give the `napstrd` user read access to files and
traverse access to their parent directories. For full-access companion downloads,
also grant it write access to the music directory. Copy music into the folder using
your normal upload tools. The daemon writes new audio files only when a
full-access companion requests a download; existing files are never overwritten.

Use a dedicated Nostr identity for this seeder: two running Napstr instances
using the same nsec replace each other's catalogue and availability events.
Keep `/etc/napstrd.env` private. The nsec is supplied only at runtime, is not
written to daemon state, and is removed from Tor's inherited environment.
Catalogue filenames, tags, and audio metadata are public; local paths and
transfer capabilities are not published.

If you use a music folder other than `/srv/napstr/music`, also run
`sudo systemctl edit napstrd` and add the following, using your actual path:

```ini
[Service]
ReadWritePaths=
ReadWritePaths=/your/music/folder
```

The unit's default music path must exist even for read-only use; replacing this
setting avoids startup failures when you select a different folder.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now napstrd
sudo systemctl status napstrd
sudo journalctl -u napstrd -f
```

No inbound firewall port is required: the transfer listener binds to loopback
and Tor creates the onion service. Allow outbound access to Tor and your Nostr
relays. Napstr starts its own Tor process; it does not use or reconfigure the
system Tor service. Nostr relay connections use normal outbound TLS, as in the
desktop app. Public Napstr peer transfers always use Tor. Paired Napstrfy
connections use encrypted Iroh, with the same direct/relay connectivity as the
desktop; Iroh audio is not routed through Tor.

On updates, stop the service, replace `/usr/local/bin/napstrd` with the verified
binary, and start it again. Preserve `/var/lib/napstrd` and `/etc/napstrd.env`.

## Connect Napstrfy over Iroh

With `napstrd` running, create a new full-access connection from the VPS:

```bash
sudo -u napstrd /usr/local/bin/napstrd pair
```

Scan the terminal QR in Napstrfy, or paste the printed `napstrfy://pair/...` URI
into its pairing screen. The wire protocol and tickets are the same as desktop
Napstr, so the existing companion needs no changes. A code expires after five
minutes, can be consumed once, and is replaced by a newer code of the same mode.
Already paired devices keep working and reconnect after daemon restarts.

For read-only access, which still allows playback and phone caching:

```bash
sudo -u napstrd /usr/local/bin/napstrd pair --read-only
```

To save a private SVG QR code, add `--qr-file /path/to/new-pairing.svg` (the command
must be able to write that path). `--json` prints machine-readable pairing data
instead of the terminal QR. Treat pairing codes as credentials until used or
expired; the service does not write them to its logs or persist pending codes.

List and revoke connections:

```bash
sudo -u napstrd /usr/local/bin/napstrd devices
sudo -u napstrd /usr/local/bin/napstrd revoke <endpoint-id>
```

These commands communicate through `/var/lib/napstrd/control.sock`, accessible
only to the daemon account and root. They do not need `NAPSTR_NSEC` or
`NAPSTR_MUSIC_DIR`, do not launch a second daemon, and expose no administration
TCP port. For a custom state directory, use, for example,
`sudo -u napstrd env NAPSTR_DATA_DIR=/your/state napstrd pair`.

The Iroh identity and paired-device permissions are stored in the daemon state
directory. Keep that directory when upgrading. Revoking a device blocks new
requests and further audio writes on its active streams. Re-pairing a device can
change its permission. Read-only devices cannot request VPS downloads or inspect
the host transfer queue.

Full-access downloads run two at a time, verify SHA-256 and audio contents before
adding files to the library, and resume queued work with fresh requests after a
restart. The queue keeps up to 200 transfers. Optional audiobook destinations
stay inside `Audiobooks/`; source filenames never select arbitrary paths.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `NAPSTR_NSEC` | Required | Nostr private key in `nsec` format |
| `NAPSTR_MUSIC_DIR` | Required | Existing absolute music/download folder; recursively scanned |
| `NAPSTR_DATA_DIR` | `/var/lib/napstrd` | Private writable state and temporary transfer storage |
| `NAPSTR_RELAYS` | Napstr's default relay list | Comma-separated `wss://` URLs |
| `NAPSTR_SCAN_INTERVAL` | `60` | Seconds between scans; range 10–86400 |
| `NAPSTR_TOR_PATH` | `tor` on `PATH` | Path to the Tor executable |

State and music directories must be separate and cannot contain each other.
One daemon can use each state directory. With the supplied systemd unit,
`StateDirectory=napstrd` permits state writes in `/var/lib/napstrd`, and
`ReadWritePaths=/srv/napstr/music` permits companion downloads there (along with
private temporary storage). To use a different state directory, also add the matching
`ReadWritePaths=/your/state/path` in a systemd override and grant ownership to
`napstrd`.

For a foreground run, export these variables in your process environment and
run `./napstrd`. `./napstrd --check` validates settings and writable state
without connecting to Nostr or Tor; `--help` lists settings and `--version`
prints the build version. `--check` also takes the state lock, so run it while
the service is stopped. SIGINT and SIGTERM stop the daemon and its Tor child.
Systemd restarts it after failures, including an unexpected Tor exit. Relay
connections reconnect automatically and failed publications are retried.

Supported audio: MP3, FLAC, WAV, Ogg Vorbis, and Opus. Invalid audio and symlinks
are excluded. New/changed files appear after a scan and catalogue publication;
availability refreshes every four minutes and expires after ten minutes.
Deleted files are refused immediately at transfer time and withdrawn from the
catalogue after the next scan. Temporary relay failures can delay publication.
The limit is 50,000 indexed audio paths, 64 outstanding offers, and four active
connections. Each transfer uses a private, hash-verified snapshot, so leave
enough free space in the state directory for up to four public peer transfers
plus four companion audio streams. Downloads temporarily use space in the music
folder until verification completes.

## Build and release

From the repository root, with a recent stable Rust toolchain (at least Rust 1.91):

```bash
cargo test --manifest-path daemon/Cargo.toml --locked
cargo build --manifest-path daemon/Cargo.toml --release --locked
./daemon/target/release/napstrd --help
```

The daemon has its own manifest, lockfile, and output directory. It compiles the
existing audio validator, Tor manager, and protocol framing modules directly;
it uses the same Iroh version and shared `remote-protocol` crate as the desktop.
It does not depend on the desktop crate or change its dependency graph.

The existing **Build Napstr desktop and Napstrfy Android installers** release
workflow calls **Build Napstr VPS daemon** for `v*` tags and manual runs with a
`release_tag`. The daemon is an independent job; it builds native x86_64 and
ARM64 static musl binaries, runs tests, and attaches archives and SHA-256
checksums to the same draft release. Desktop/Android jobs keep their existing
build steps. No nsec or music path is needed in Actions.

Publishing a GitHub release also triggers the daemon workflow. For a daemon-only
test, manually run **Build Napstr VPS daemon** with an empty `release_tag` and
download its artifacts. To attach a daemon-only build to an existing release,
select the matching tag as the workflow ref and supply that same `release_tag`.
Relevant pull requests run the daemon builds and tests without uploading to a
release.
