# Napstr protocol

This document specifies the public protocol implemented by Napstr clients. It
contains the event kinds, tags, messages, Tor setup, and file-transfer framing
needed to build an interoperable client.

Napstr uses two version identifiers:

- `napstr/1` identifies catalogue and private negotiation messages.
- Transfer protocol version `2` identifies the TCP protocol carried over Tor.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative.

## Protocol overview

| Function | Transport or standard |
| --- | --- |
| Identity and profiles | Nostr, kind `0` |
| DM relay discovery | Nostr, kind `10050` |
| Audio catalogue | Nostr, kind `30421` |
| Seeder availability | Nostr, kind `30422` |
| Audiobook collections | Nostr, kind `30423` |
| Private download negotiation | NIP-17 and NIP-59 |
| Public chat and track discussions | NIP-C7, kind `9` |
| Reports | NIP-56, kind `1984` |
| File transfer | TCP through a temporary Tor v3 onion service |
| Optional phone companion | Iroh QUIC with ALPN `/napstr/mobile/1` |
| File identity and integrity | SHA-256 of the complete file bytes |

Nostr events use the standard NIP-01 event format and signature validation.
Clients MUST reject events with an invalid signature or ID.

Relevant standards:

- [NIP-01: basic protocol](https://github.com/nostr-protocol/nips/blob/master/01.md)
- [NIP-17: private direct messages](https://github.com/nostr-protocol/nips/blob/master/17.md)
- [NIP-40: expiration timestamps](https://github.com/nostr-protocol/nips/blob/master/40.md)
- [NIP-50: relay search](https://github.com/nostr-protocol/nips/blob/master/50.md)
- [NIP-56: reporting](https://github.com/nostr-protocol/nips/blob/master/56.md)
- [NIP-59: gift wrapping](https://github.com/nostr-protocol/nips/blob/master/59.md)
- [NIP-C7: chats](https://github.com/nostr-protocol/nips/blob/master/C7.md)

## Identity, profiles, and relays

Each installation uses a normal Nostr secp256k1 identity. A client MAY generate
a random identity on first launch or let the user supply an existing key. Secret
keys MUST be held in secure local storage and MUST never be placed in an event,
log, URL, catalogue entry, or transfer message.

Napstr publishes standard kind `0` profile metadata. The current fields are:

```json
{
  "name": "napstr-user",
  "display_name": "napstr-user",
  "about": "Sharing files privately with Napstr. napstr.net",
  "picture": "https://example.com/avatar.png"
}
```

`picture` is optional. The reference client accepts only HTTPS picture URLs.
Profile edits are published when the profile changes.

Relay selection is user-configurable. The reference defaults are:

```text
wss://relay.damus.io
wss://nos.lol
wss://relay.nostr.com
wss://relay.primal.net
wss://relay.snort.social
wss://nostr.mom
wss://relay.nostr.band
```

After connecting, a client publishes the relays on which it accepts private
messages as a kind `10050` event:

```json
{
  "kind": 10050,
  "content": "",
  "tags": [
    ["relay", "wss://relay.example.com"],
    ["relay", "wss://another.example.com"]
  ]
}
```

Clients should publish to and query all configured relays, tolerate duplicate
events, and deduplicate by Nostr event ID and public key where appropriate.

## File identity and audio policy

The `fileId` is the lowercase hexadecimal SHA-256 digest of the complete file
bytes. It is exactly 64 hexadecimal characters. Identical bytes therefore share
one catalogue identity and may have multiple seeders.

Interoperable clients MUST support these catalogue claims:

| Extension | `format` | `mime` |
| --- | --- | --- |
| `.mp3` | `MP3` | `audio/mpeg` |
| `.flac` | `FLAC` | `audio/flac` |
| `.wav` | `WAV` | `audio/wav` |
| `.ogg` | `OGG` | `audio/ogg` |
| `.opus` | `OPUS` | `audio/ogg` |

A file MUST be validated from its bytes as MP3, FLAC, PCM WAV, Ogg Vorbis, or
Opus before publication and again after download. An extension or MIME claim by
itself is insufficient. Embedded audio artwork is allowed.

Only the selected shared folder and its descendants are indexed. Folder names
and local paths are not published. A public filename MUST be a basename, never
a path, and a receiving client MUST treat it as untrusted text.

## Catalogue event: kind 30421

Kind `30421` is an addressable event. Its `d` tag is the file ID, so the newest
valid event from one author for that `d` value replaces the author's previous
entry.

Required tags:

```json
[
  ["d", "<fileId>"],
  ["t", "napstr"],
  ["x", "<fileId>"],
  ["name", "song.mp3"],
  ["size", "1234567"],
  ["m", "audio/mpeg"],
  ["t", "song"],
  ["alt", "Napstr shared file catalogue entry"]
]
```

- `d`, `x`: the lowercase SHA-256 file ID.
- `t`: the literal catalogue marker `napstr`, plus lowercase search words.
- `name`: the public filename basename.
- `size`: file size in bytes as a base-10 string.
- `m`: supported audio MIME type.
- `alt`: human-readable event description.

A catalogue event contains at most 20 distinct search-word `t` tags, each no
longer than 32 characters. Tokens are split on non-alphanumeric characters and
derived from the filename, embedded title, artist, album, and user tags. These
hashtags are indexed by ordinary NIP-01 relays and provide the portable
exact-word search path. Clients omit common stop words such as `the`, `a`,
`and`, `of`, `on`, `in`, `to`, `for`, and `with` from these search tags.

The event content is JSON with camel-case property names:

```json
{
  "protocol": "napstr/1",
  "fileId": "c893b2b1206667a573ddb335ce550a3b7a06b28617ddb8e61f21d7d6c8f09abc",
  "filename": "song.mp3",
  "title": "Song Title",
  "artist": "Artist Name",
  "album": "Album Name",
  "format": "MP3",
  "mime": "audio/mpeg",
  "size": 1234567,
  "license": "unspecified",
  "description": "",
  "tags": "punk,live"
}
```

For protocol version `napstr/1`, `filename` is the authoritative public file
basename. `title`, `artist`, and `album` are optional, bounded text read from
the audio file's embedded metadata. `description` is empty and `license` is
`unspecified`. `tags` is an optional comma-separated user search field.

The reference tag rules are:

- no more than 12 comma-separated tags;
- no more than 32 characters per tag;
- no more than 256 characters in total;
- no control characters;
- duplicates are removed case-insensitively.

Named searches issue a separate bounded indexed `t`-hashtag filter for up to
four meaningful query words. This prevents one common word from consuming the
relay limit for every other term:

```json
{"kinds":[30421],"#t":["metallica"],"limit":500}
```

Clients MAY issue a bounded NIP-50 query alongside this for partial-word,
fuzzy, or better-ranked matches:

```json
{"kinds":[30421],"#t":["napstr"],"search":"metallica","limit":500}
```

Clients MUST verify the returned filename, title, artist, album, and tags
locally because hashtag values are OR filters and NIP-50 support and matching
behavior vary between relays. Clients SHOULD retain previously verified entries
in a local cache.

An empty search MUST NOT enumerate an unbounded public catalogue. Consumers
first obtain unexpired availability events, deduplicate file IDs, rank them by
distinct active authors, and select a bounded window. Previously validated
catalogue records for active IDs SHOULD be hydrated from the local cache
immediately. Only active IDs whose current seeder records are missing from that
cache are requested from relays, using standard NIP-01 `d`-tag filters in
bounded pages:

```json
{"kinds":[30421],"#t":["napstr"],"#d":["<fileId1>","<fileId2>"],"limit":500}
```

The reference client selects at most 10,000 active IDs, requests no more than
500 missing IDs per progressive page, and retains browse state for 10 minutes.
Named searches issue indexed `t`-tag and NIP-50 queries concurrently before
applying local validation and matching. A random-track feature uses its own
50-ID window rather than hydrating the full browse window.

Clients may display the number of distinct, valid file IDs in the current
unexpired availability heartbeats before every corresponding catalogue event
has been fetched. This is an availability total, not a permanent global track
count, and changes as heartbeat events expire or seeders connect and disconnect.

They MUST validate the event signature, `protocol`, `fileId`, filename, size,
format, MIME type, and tags before displaying an entry. Entries with the same
`fileId` are aggregated, with each distinct author becoming a possible seeder.
Search is case-insensitive over `filename`, `title`, `artist`, `album`, and
`tags`. Results should be ranked by the number of currently available seeders.

### Catalogue withdrawal

When a file is no longer shared, its author replaces the catalogue event at the
same kind and `d` coordinate with:

```json
{
  "kind": 30421,
  "tags": [
    ["d", "<fileId>"],
    ["t", "napstr"]
  ],
  "content": "{\"protocol\":\"napstr/1\",\"deleted\":true}"
}
```

Clients MUST treat the latest event at this coordinate as withdrawn and MUST
not offer the older entry from that author.

## Audiobook manifest: kind 30423

Kind `30423` is an addressable collection event layered over ordinary kind
`30421` files. Every chapter MUST still be published as an independent catalogue
entry and transferred with the normal Napstr file protocol. Clients that do not
support audiobook manifests therefore remain able to find and download the
chapters individually.

The manifest `d` tag and `audiobookId` are the lowercase SHA-256 of the bytes
`napstr-audiobook-v1\0` followed by every ordered chapter file ID in hexadecimal
text. Changing the files or their order creates a new edition ID.

Required tags include:

```json
[
  ["d", "<audiobookId>"],
  ["t", "napstr-audiobook"],
  ["x", "<audiobookId>"],
  ["title", "Book title"],
  ["alt", "Napstr audiobook manifest"]
]
```

Search-word `t` tags follow the catalogue token rules and may be derived from
the book title, author, narrator, and chapter names. The content is:

```json
{
  "protocol": "napstr/1",
  "audiobookId": "<64 lowercase hex characters>",
  "title": "Book title",
  "author": "Author",
  "narrator": "Narrator",
  "totalSize": 123456789,
  "chapters": [
    {
      "position": 1,
      "fileId": "<chapter SHA-256>",
      "filename": "01 - Chapter One.mp3",
      "title": "Chapter One",
      "format": "MP3",
      "mime": "audio/mpeg",
      "size": 1234567
    }
  ]
}
```

A manifest MUST contain between 1 and 500 unique chapters. Positions MUST be
contiguous and start at 1, `totalSize` MUST equal the sum of chapter sizes, each
filename MUST be a basename, and every chapter claim MUST satisfy the normal
audio catalogue rules. The reference client limits manifest content to 128 KiB.
Clients MUST validate the event signature and recompute
the edition ID before displaying the collection.

For a complete audiobook stored in one audio file, the reference client creates
a one-chapter manifest when the publisher adds the exact, case-insensitive track
tag `audiobook`. This does not replace or suppress the file's ordinary kind
`30421` catalogue entry.

The reference client also uses a top-level local directory named `Audiobooks`.
It creates that directory only when it is missing and never replaces or clears
an existing directory. Each immediate child directory becomes one recursively
ordered manifest, while each loose audio file directly inside `Audiobooks`
becomes a one-chapter manifest. This filesystem convention is local UI
behaviour, not a wire-protocol requirement; all books use the same kind `30423`
format above. Publishers may explicitly group folders elsewhere in the shared
library too.

A publisher is a complete active seeder for an audiobook only while its current
kind `30422` availability events contain every chapter file ID. A client MAY
download chapters from different complete publishers, but MUST preserve manifest
order. Downloaded chapters SHOULD be stored together and played in that order.

When a local collection is removed or its edition ID changes, its author replaces
the previous coordinate with the same deletion content used for catalogue
withdrawals and the tags `d=<old audiobookId>` and `t=napstr-audiobook`.

Audiobook manifests are additive catalogue assertions. Clients MUST NOT use a
manifest to remove, suppress, or reclassify its kind `30421` chapter entries in
ordinary audio search. This ensures that an inaccurate or malicious collection
published by one identity cannot hide regular tracks or alter another
publisher's catalogue.

## Availability event: kind 30422

Catalogue publication and live availability are separate. Kind `30422` is an
addressable heartbeat containing file IDs the author is currently prepared to
seed.

The reference client divides IDs into groups of at most 400 and publishes one
event per group:

```json
{
  "kind": 30422,
  "tags": [
    ["d", "availability-0000"],
    ["t", "napstr-availability"],
    ["expiration", "1787419200"]
  ],
  "content": "[\"<fileId1>\",\"<fileId2>\"]"
}
```

- `d` is `availability-` followed by a zero-padded, four-digit group index.
- `t` is the literal value `napstr-availability`.
- `expiration` is a Unix timestamp 10 minutes after publication.
- `content` is a JSON array of lowercase file IDs.

The reference client publishes immediately after its catalogue and every four
minutes while connected. With no shared files it publishes an empty first
group, allowing its previous first group to be replaced.

While a large folder is still being indexed, the reference client may also
publish short-lived incremental availability groups. Their `d` tag is
`availability-delta-<uuid>-<four-digit group index>`. Each contains at most 400
newly verified file IDs and uses the same tag, content, and 10-minute expiration
rules as a regular heartbeat. A later full heartbeat covers the complete
library; consumers treat both forms identically when building active pairs.

Consumers query kind `30422` by the `napstr-availability` tag, discard expired
events, and build active `(author public key, fileId)` pairs. An author is a live
seeder for a catalogue entry only when its unexpired availability event contains
that file ID. Older surplus groups disappear naturally through expiration.

## Public chats: NIP-C7 kind 9

Napstr public chat uses portable NIP-C7 kind `9` events. It does not depend on a
relay-owned NIP-29 group.

### Trollbox

```json
{
  "kind": 9,
  "content": "hello",
  "tags": [
    ["t", "napstr-trollbox"],
    ["client", "Napstr"],
    ["alt", "Public message in the Napstr trollbox"]
  ]
}
```

### Track discussion

A track topic is `napstr-` followed by its lowercase 64-character file ID:

```json
{
  "kind": 9,
  "content": "great recording",
  "tags": [
    ["t", "napstr-<fileId>"],
    ["client", "Napstr"],
    ["alt", "Public message in a Napstr track discussion"]
  ]
}
```

Messages are public and signed. Clients SHOULD fetch the newest 100 for a topic
and display them oldest first. The reference client accepts 1–500 characters,
requires a single line, rejects unsafe control and bidirectional formatting
characters when sending, and sanitizes them when displaying. Locally blocked
authors are omitted.

## Private download negotiation: NIP-17

Download negotiation uses NIP-17 private direct messages. The JSON below is the
content of the private-message rumor, not a public relay event. NIP-17 uses a
kind `14` rumor protected with NIP-44 encryption and NIP-59 wrapping; relays see
a kind `1059` gift wrap rather than the plaintext message.

Every private signal carries these rumor tags:

```json
[
  ["expiration", "<Unix time 20 minutes from now>"],
  ["client", "Napstr"]
]
```

A receiver MUST unwrap only messages addressed to its own key, validate the
Nostr events, require kind `14`, require an unexpired `expiration`, and reject
unknown Napstr messages.

### Download request

The downloader selects up to three distinct active seeders and sends each one:

```json
{
  "type": "DOWNLOAD_REQUEST",
  "protocol": "napstr/1",
  "request_id": "42f9ac7c-fd56-475c-9a6d-adcc35a1f826",
  "file_id": "<fileId>"
}
```

`request_id` is a fresh UUID v4 generated by the downloader. A seeder MUST
confirm that `file_id` is still present in its indexed shared folder, is not
blocked, and still passes audio validation.

### Download offer

An accepting seeder replies:

```json
{
  "type": "DOWNLOAD_OFFER",
  "protocol": "napstr/1",
  "offer": {
    "requestId": "42f9ac7c-fd56-475c-9a6d-adcc35a1f826",
    "fileId": "<fileId>",
    "onion": "<56 lowercase base32 characters>.onion",
    "port": 80,
    "capability": "<64 lowercase hexadecimal characters>",
    "expiresAt": 1787419500
  }
}
```

Offer property names are camel case. `capability` is 32 cryptographically
random bytes encoded as lowercase hex. `expiresAt` is 15 minutes after the
offer is made.

The downloader MUST reject an offer when:

- it is expired;
- its onion is not a syntactically valid v3 onion hostname;
- its request ID or file ID does not match an outstanding request;
- its sender was not one of the explicitly requested seeders;
- the file or sender is locally blocked.

### Download refusal

A seeder unable or unwilling to serve the request replies:

```json
{
  "type": "DOWNLOAD_REFUSED",
  "protocol": "napstr/1",
  "request_id": "42f9ac7c-fd56-475c-9a6d-adcc35a1f826",
  "file_id": "<fileId>",
  "reason": "requested file ID is not currently shared"
}
```

The request, onion address, capability, and refusal remain inside NIP-17. They
MUST NOT be published in the public catalogue or chat.

## Tor session and onion-service flow

Napstr uses a client-controlled Tor process. A packaged client may bundle Tor;
another implementation may use a compatible local Tor binary. There is no
direct-IP transfer fallback.

The reference Tor process is launched with equivalent options:

```text
--DataDirectory <new private session directory>
--SocksPort auto
--ControlPort auto
--ControlPortWriteToFile <session file>
--CookieAuthentication 1
--CookieAuthFile <session cookie file>
--ClientOnly 1
--AvoidDiskWrites 1
--Log notice stdout
```

The client waits for `status/bootstrap-phase` to report 100 percent and obtains
the SOCKS listener through `GETINFO net/listeners/socks`. The Tor process stays
available for the application session and is stopped when the application
closes. Temporary Tor data is removed after shutdown.

To seed files:

1. Bind a TCP listener to an OS-selected port on `127.0.0.1` only.
2. Authenticate to Tor's control port using its cookie.
3. Send:

   ```text
   ADD_ONION NEW:BEST Flags=DiscardPK Port=80,127.0.0.1:<local-port>
   ```

4. Keep that authenticated control connection open for the onion service's
   lifetime.
5. Send the returned v3 onion hostname only in a valid private download offer.

`NEW:BEST` creates a fresh onion key and `DiscardPK` prevents the private onion
key from being returned. The reference client reuses one app-session onion for
its active offers. The onion disappears when its control connection closes.

The seeder stores only `SHA256(capability)` as its authorization lookup key and
binds the grant to the requested file ID, requester, and expiration time. The
raw capability is disclosed only to the downloader. Expired grants are removed.
The reference client permits at most 64 simultaneous active grants and one
active grant per requester/file pair.

The downloader connects to `<onion>:80` through its local Tor SOCKS5 listener.
It MUST NOT resolve or connect to the peer outside Tor. Multiple valid offers
may be tested for responsiveness; the first valid responsive source transfers
the file, while another offer can be used if that source fails.

## Transfer protocol version 2

The transfer is a TCP byte stream over the onion service. Each control frame is:

1. a four-byte unsigned big-endian payload length;
2. that many bytes of UTF-8 JSON.

Control JSON MUST be between 1 and 65,536 bytes. Frame `type` values are uppercase.
After a `FILE_DATA` frame, exactly the declared number of raw file bytes follows.

### Frame sequence

The downloader sends:

```json
{
  "type": "HELLO",
  "version": 2,
  "capability": "<offer capability>",
  "file_id": "<fileId>"
}
```

The seeder verifies the protocol version and the hashed capability grant, checks
that it authorizes this file and has not expired, and confirms the file is still
shared and valid. It replies:

```json
{
  "type": "WELCOME",
  "version": 2,
  "file_id": "<fileId>",
  "filename": "song.mp3",
  "size": 1234567
}
```

The downloader MUST verify the version, file ID, and catalogue size before
requesting any bytes. It then sends:

```json
{"type":"REQUEST_FILE"}
```

The seeder replies with a control frame:

```json
{
  "type": "FILE_DATA",
  "size": 1234567,
  "sha256": "<fileId>"
}
```

Immediately after this frame, the seeder writes exactly `size` raw bytes. It
computes SHA-256 while reading and aborts if the local file no longer matches
`fileId`.

The downloader writes to a temporary local file, reads exactly `size` bytes,
computes SHA-256, and MUST reject the result unless the digest equals `fileId`.
It also MUST repeat audio-content validation before making the file available.
A safe destination uses only a sanitized basename and must not overwrite an
unrelated existing file.

After successful verification, the downloader sends:

```json
{"type":"TRANSFER_COMPLETE"}
```

The seeder replies:

```json
{"type":"TRANSFER_COMPLETE"}
```

It then invalidates the capability grant. A downloader may stop an unfinished
transfer with:

```json
{"type":"CANCEL"}
```

The seeder invalidates the grant on cancellation.

Errors are control frames:

```json
{
  "type": "ERROR",
  "code": "UNAUTHORIZED",
  "message": "capability is invalid or expired"
}
```

`BAD_HELLO` and `UNAUTHORIZED` are currently defined error codes. Clients MUST
handle any `ERROR` code without treating its message as trusted markup.

Reference timeouts are 30 seconds for `HELLO`, 60 seconds for manifest and data
headers, 45 seconds without file progress, and 15 minutes for an idle authorized
connection.

## Optional Napstrfy companion protocol

The phone companion is separate from Napstr's public Nostr/Tor protocol. It
connects only to the user's own running desktop over Iroh, using ALPN
`/napstr/mobile/1`. Iroh authenticates both endpoints by Ed25519 endpoint ID and
encrypts QUIC traffic end to end; a relay may forward ciphertext when a direct
path is unavailable.

The desktop displays a URI with this shape as a QR code:

```text
napstrfy://pair/<unpadded-base64url-encoded-ticket-json>
```

The decoded ticket uses camel-case fields:

```json
{
  "version": 1,
  "endpointId": "<desktop Iroh endpoint ID>",
  "endpointAddr": "<JSON-encoded Iroh EndpointAddr>",
  "token": "<32 random bytes as lowercase hex>",
  "expiresAt": 1787680200,
  "desktopName": "Napstr"
}
```

The token expires after five minutes and MUST be consumed by the first
successful pairing. The desktop stores the authenticated phone endpoint ID in
an allowlist. The phone persists its own Iroh secret key and the desktop address,
but MUST NOT persist the one-use token. Removing the phone from Napstr deletes
that allowlist entry.

Each operation uses one Iroh bidirectional stream. A control frame is a
four-byte unsigned big-endian length followed by UTF-8 JSON, with a maximum
length of 262,144 bytes. Request and response objects use a camel-case `type`
discriminator. Defined requests are:

```json
{"type":"pair","token":"<token>","deviceName":"Napstrfy on Android phone"}
{"type":"library","query":"metallica","offset":0,"limit":100}
{"type":"search","query":"enter sandman"}
{"type":"requestDownload","fileId":"<fileId>","sourcePubkeys":["<Nostr pubkey>"],"destinationFolder":"<optional audiobook folder>"}
{"type":"transfers"}
{"type":"fetchAudio","fileId":"<fileId>"}
{"type":"available","fileIds":["<fileId>","<fileId>"]}
{"type":"status"}
{"type":"ping"}
```

Library pages are limited to 200 items and searches to 120 characters. Search
and download requests are executed by the desktop's normal Nostr and Tor
services; the phone never receives the desktop Nostr secret key, Tor onion
capabilities, or filesystem paths.

`available` accepts at most 200 SHA-256 file IDs and returns the subset still
present in the desktop's indexed Napstr folder. Napstrfy uses this bounded check
after reconnecting to reconcile its verified offline audio cache. A companion
MUST preserve cached files when paired with an older desktop that does not
support this request.

`destinationFolder` is optional and is used for audiobook chapters. The desktop
accepts only one sanitized path component and stores it beneath
`Napstr/Audiobooks/`; it never accepts an absolute path or nested path from the
companion. Omitting it keeps the normal music destination at the Napstr-folder
root.

Responses have `type` values `paired`, `library`, `search`, `status`,
`downloadRequested`, `transfers`, `audioReady`, `available`, `pong`, or `error`. Track
objects contain `fileId`, `filename`, `title`, `artist`, `album`, `format`,
`mime`, `size`, `tags`, `local`, and a `sources` array whose entries contain
only `pubkey` and `displayName`.

The lightweight `status` response contains `libraryRevision`, a monotonically
increasing local-library revision. A companion MAY poll it and should reload
library pages only when it changes. The revision check carries no catalogue
rows and does not affect active audio streams.

For `fetchAudio`, an `audioReady` control frame is immediately followed on the
same receive stream by exactly `track.size` raw bytes and then stream finish.
The desktop MUST serve only an indexed supported-audio path contained by the
currently selected Napstr folder. The phone writes to a temporary cache file,
MUST reject excess or truncated bytes, and MUST verify that the complete
SHA-256 digest equals `track.fileId` before playback. All other responses end
after their control frame.

## Reports and local blocking

Napstr supports NIP-56 report events even when a client does not expose a report
button. A report is kind `1984`:

```json
{
  "kind": 1984,
  "content": "reason, between 1 and 500 characters",
  "tags": [
    ["p", "<seeder public key>", "malware"],
    ["e", "<catalogue event ID>", "malware"],
    ["x", "<fileId>", "malware"],
    ["client", "Napstr"]
  ]
}
```

Supported report types are `illegal`, `malware`, `spam`, `nudity`, `profanity`,
`impersonation`, and `other`.

Blocking a file ID or public key is local state, not a Napstr Nostr event. A
client should omit blocked catalogue entries, ignore blocked availability and
offers, refuse blocked requesters, and hide public-chat messages from blocked
authors.

## Privacy and security requirements

Public Nostr data includes:

- the user's public key and profile;
- public filenames, sizes, formats, MIME types, tags, and SHA-256 file IDs;
- catalogue and live-availability events;
- trollbox and track-discussion messages.

Private NIP-17 data includes the requested file ID, request ID, onion address,
capability, expiration, and refusal reason. The file bytes and transfer frames
travel through Tor.

An interoperable client MUST:

- validate every Nostr event ID and signature;
- validate exact 64-character hexadecimal file IDs;
- accept only valid 56-character v3 onion service IDs plus `.onion`;
- bind every offer to its expected request, file, and seeder;
- enforce expirations and make capabilities single-purpose secrets;
- connect to peers only through Tor, with no clearnet fallback;
- verify the signed catalogue size and complete SHA-256 digest;
- validate downloaded audio from its contents;
- treat public filenames, tags, names, chat, and error text as hostile text;
- use only a safe basename for local destination paths;
- keep shared-folder paths and secret keys out of public and private messages;
- remove stale local catalogue entries when the shared folder changes.

Tor prevents file-transfer peers from receiving one another's direct IP address,
but relays still observe Nostr connections and public events, and a network
provider may observe that Tor is being used. SHA-256 verifies byte identity and
integrity; it does not establish authorship, licensing, or trustworthiness.

## Interoperability checklist

A minimal independent Napstr client is interoperable when it can:

1. connect a Nostr identity to configured relays and advertise kind `10050` DM
   relays;
2. publish and replace kinds `30421` and `30422` exactly as specified;
3. aggregate signed catalogue events by file ID and active author;
4. send, receive, unwrap, expire, and validate the three NIP-17 signal messages;
5. create or connect to temporary v3 onions without a direct-IP fallback;
6. implement transfer protocol version `2` and its length-prefixed JSON frames;
7. verify file size, SHA-256, and audio content before accepting a download;
8. publish and read kind `9` trollbox and per-track discussion events;
9. withdraw files that are no longer present in the selected shared folder.
