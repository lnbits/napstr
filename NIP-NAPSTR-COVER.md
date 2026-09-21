# Napstr cover event: kind `30427`

> **Status:** draft proposal for upstream inclusion in
> [lnbits/napstr `PROTOCOL.md`](https://github.com/lnbits/napstr/blob/main/PROTOCOL.md).
> First implemented (consumer + NIP-07 curator publisher) by napstr.fm.
> Normative keywords follow the
> main document: **MUST**, **MUST NOT**, **SHOULD**, **MAY**.
>
> **Kind selection (surveyed 2026-09-10, occupancy re-probed 2026-09-21):**
> `30427` was the first kind in this range with no occupant when it was chosen.
> The range is a shared commons rather than a Napstr reservation: every
> coordinate from `30420` upward is claimed by at least one unrelated
> application, and `30425` was quiet only temporarily — an unrelated protocol
> resumed publishing there on 2026-09-15. `30427` itself has since acquired three
> foreign events as well. Marker tags (`#t=napstr-cover`) are therefore not
> decorative: they are the only thing distinguishing Napstr events from neighbours
> at the same coordinate, so publishers and consumers MUST filter on the marker
> and never on the kind alone. See "Neighbourhood occupancy" below.

Album artwork is cosmetic metadata that every browsing client otherwise resolves
independently from rate-limited third-party APIs (MusicBrainz, Cover Art
Archive, iTunes Search). This event lets publishers share a resolution once so
every other client inherits it from relays alongside the catalogue. Cover events
are **additive assertions**: they never alter, suppress, or reclassify kind
`30421` catalogue entries, and a client that ignores this kind entirely remains
fully interoperable.

Kind `30427` is a parameterized-replaceable (addressable) event. Its `d` tag is
a normalized album key, so the newest valid event from one author for that key
replaces the author's previous cover claim for that album.

## Event shape

Required tags:

```json
[
  ["d", "<coverKey>"],
  ["t", "napstr-cover"],
  ["alt", "Napstr album cover assertion"]
]
```

Optional tags:

```json
[
  ["x", "<coverFileId>"],
  ["thumb", "<https image url>"],
  ["m", "image/jpeg"],
  ["client", "Napstr"]
]
```

- `d`: the **cover key** — see normalization rules below.
- `t`: the literal marker `napstr-cover`. Cover events carry no search-word
  `t` tags; discovery is by marker + `d` filter only.
- `x`: when present, the lowercase SHA-256 of a **cover image file** that the
  author also publishes as an ordinary kind `30421` catalogue entry (see
  "Embedded covers" below).
- `thumb`: an HTTPS URL to a smaller rendition of the same image. Clients MAY
  prefer it in dense grids and fall back to `art`.
- `m`: the MIME type of the `x` image (`image/jpeg`, `image/png`, or
  `image/webp`). SHOULD be present whenever `x` is present.
- `alt`: human-readable event description, e.g. `Album cover for <album> by
  <artist>`.

## Cover key normalization

The `d` value is the album's display metadata joined by a single pipe:

```text
coverKey = trim(artist) + "|" + trim(album), all lowercased
```

- Lowercasing uses Unicode-aware case folding (`toLowerCase()`).
- No other normalization is applied: whitespace runs, diacritics, punctuation,
  and edition markers (`(Deluxe Edition)`, `[2023 Remaster]`, `- Disc 2`) are
  preserved verbatim. Publishers MUST NOT strip or rewrite the embedded
  metadata; the key exists to match catalogue display strings byte-for-byte,
  not to canonicalize releases.
- The artist half is the track `artist` field as published in kind `30421`
  content; the album half is the track `album` field. Both are required — an
  event whose key is missing either side of the pipe is invalid.
- Total key length MUST NOT exceed 300 characters.

## Canonical alias

Real-world catalogues contain mis-tagged files: watermarks (`! www.example.tk !`),
filenames-as-albums, and edition markers (`(Deluxe Edition)`). A cover published
against a clean key would then miss those entries, and vice versa.

The canonical alias recomputes the key from **cleaned** display metadata:

```text
cleanArtist  = strip leading URL/watermark tokens and bracket characters
cleanAlbum   = strip "(Deluxe)", "[Remaster]", "- Disc N", etc.
canonicalKey = norm(cleanArtist) + "|" + norm(cleanAlbum)
```

Publishers SHOULD include `c=<canonicalKey>` when it differs from `d`.
Consumers SHOULD query both their verbatim cover key and their locally computed
canonical key in the same `#d` filter, and MUST treat a cover event arriving via
the canonical alias exactly as if it had matched the verbatim key.

## Content

The content is JSON with camel-case property names, at most 4 KiB:

```json
{
  "protocol": "napstr/1",
  "art": "https://is1-ssl.mzstatic.com/image/thumb/Music/.../600x600bb.jpg",
  "mbid": "f4a7b0d2-...",
  "year": "2007",
  "genre": "Rock",
  "collection": "City of Echoes",
  "source": "itunes"
}
```

- `protocol`: the literal `napstr/1`.
- `art`: HTTPS URL of the front cover image. REQUIRED when the event carries
  no `x` tag; OPTIONAL (as a hotlink mirror) when `x` is present. Only HTTPS
  URLs are accepted; clients MUST reject `http:` and non-URL values.
- `mbid`: MusicBrainz **release-group** MBID the art was resolved from, when
  known. Lets consumers deep-link or re-resolve at other sizes without a new
  search.
- `year`: four-digit first-release year, when known.
- `genre`: single primary genre label, when known.
- `collection`: canonical release title (e.g. the MusicBrainz release-group
  title), when the resolved release differs from the catalogue `album` string.
- `source`: provenance hint — `itunes`, `musicbrainz`, `embedded`, `manual`,
  or another short lowercase token. Informational only; MUST NOT be used for
  trust decisions (anyone can claim any source).

Unknown properties MUST be ignored.

## Linked covers

The common case: the author resolved art from a third-party API and publishes
only the URL. Consumers fetch the image lazily (`<img loading="lazy">` or
equivalent), SHOULD cache it locally, and MUST tolerate unreachable or
replaced URLs by falling back to their own external lookup or placeholder.

Consumers SHOULD bound per-host image concurrency and MUST NOT treat a broken
`art` URL as a reason to penalize the author — URLs rot.

### Sizes

`art` SHOULD be a bounded rendition rather than whatever the source API hands
back. Around 1200 pixels on the long edge is the working rule: that covers a
phone's album header, which draws at roughly 750 device pixels, and a desktop
window's largest art surface, without shipping a scan. Cover sources return the
file that was uploaded — commonly 1500 pixels and up, sometimes several
megabytes — so a link to the original makes every consumer on the network pay for
detail no screen will draw. Where the source offers renditions, ask for one: the
Cover Art Archive serves 250, 500 and 1200 pixel versions of every image, and the
1200 is what `art` wants. Note that its `large` alias means 500, so ask by number.

`thumb` is the counterpart for dense grids: 250 pixels on the long edge fills a
shelf tile, a list row, or a blurred backdrop, and it is the rendition a client
can paint while the larger one is still arriving. Publishers SHOULD supply both
when they can, because a client that has only `art` has nothing to show until the
larger image lands.

Neither bound can be checked before the image is fetched, so these are publisher
obligations rather than validation rules: a consumer MUST NOT reject a cover for
an `art` URL it cannot measure, and SHOULD prefer `thumb` wherever the display is
small enough that the difference cannot be seen.

## Embedded covers

Publishers whose local audio files contain embedded artwork MAY share the
image itself through Napstr:

1. Extract the front-cover image as its own file.
2. Compute its `fileId` (lowercase SHA-256 of the complete bytes) exactly as
   for audio files.
3. Publish it as a normal kind `30421` catalogue entry so existing
   availability heartbeats, search, and the Tor transfer protocol serve it
   unchanged. The entry's `format`/`mime` describe the image (`JPG`, `PNG`,
   `WEBP`). Clients that only accept audio formats simply ignore the entry.
4. Include the image's `fileId` in the cover event's `x` tag, with `m` set to
   the image MIME type.

A consumer that wants the embedded cover requests `<coverFileId>` through the
normal NIP-17 download negotiation and transfer protocol from any active
seeder of that file, verifies the SHA-256, and validates the bytes as a
JPEG/PNG/WebP image **from their contents** (magic bytes), never from the
extension or MIME claim alone. `art`, when also present, is the fast path; `x`
is the self-healing path that survives third-party link rot.

Image catalogue entries SHOULD stay small; publishers SHOULD downscale covers
to at most 1200×1200 before sharing.

## Trust and conflict resolution

Cover events are unsigned-attribution, signed-origin assertions: the signature
proves authorship, not accuracy. Any identity may publish a cover for any
album key. Consumers apply this order:

1. An event whose author is a **currently active seeder of at least one track
   of that album** (present in an unexpired kind `30422` heartbeat covering a
   `30421` entry whose normalized `artist|album` equals the cover key) wins
   over events from non-seeders.
2. Within the same trust class, newest `created_at` wins.
3. Events from locally blocked authors are ignored (see "Reports and local
   blocking" in the main document). Wrong or abusive covers SHOULD be reported
   as an ordinary NIP-56 kind `1984` report, with the tag meanings the main
   document defines:

   ```json
   {
     "kind": 1984,
     "content": "reason, between 1 and 500 characters",
     "tags": [
       ["p", "<cover author public key>", "spam"],
       ["e", "<cover event ID>", "spam"],
       ["napstr-cover", "<artist|album>"],
       ["client", "Napstr"]
     ]
   }
   ```

   `e` names the claim being reported, `p` names its author, and
   `napstr-cover` carries the album key that claim answers.

   `x` is **not** used for the album key. NIP-56 defines `x` as the SHA-256 of
   the reported content, and a consumer following it would read a key as a
   content hash. A cover report MAY carry `x` only when the reported claim
   publishes the image's own file ID in its `x` tag, in which case the report
   repeats that same value under the same report type.

   The album key is carried explicitly rather than left to be derived from `e`
   because kind `30427` is addressable: its author may replace the claim at the
   same `d` coordinate at any time, which changes the event ID and orphans a
   report that named only that. A report that arrives without the claim it
   names — from a relay the consumer does not read, or after the claim was
   replaced — is still meaningful while the key is present. A consumer that
   holds the claim MAY match on `e` instead.

A consumer that holds a valid seeder-authored event MAY ignore later
non-seeder events for the same key until the seeder's event is replaced by its
author.

## Consumption

Typical flow for a catalogue browser:

1. Subscribe once for the marker:

   ```json
   {"kinds":[30427],"#t":["napstr-cover"]}
   ```

2. As kind `30421` catalogue entries arrive, compute each album's cover key
   **and canonical alias**, then batch-fetch misses by `d` filter, mirroring
   the catalogue's bounded paging:

   ```json
   {"kinds":[30427],"#t":["napstr-cover"],"#d":["<coverKey1>","<canonicalKey1>","<coverKey2>","<canonicalKey2>"],"limit":500}
   ```

   The reference consumer chunks `d` filters at 75 values and at most 4
   concurrent requests.

3. Merge the winning event's fields into the local cover cache keyed by cover
   key. A cover event satisfies the key: consumers SHOULD NOT additionally
   query third-party APIs for art of an album a cover event already provides.

4. External lookup (MusicBrainz, iTunes, etc.) remains the fallback for albums
   with no cover event, and stays client-optional.

## Withdrawal

An author retracts a cover claim by replacing the same coordinate with the
catalogue withdrawal body:

```json
{
  "kind": 30427,
  "tags": [
    ["d", "<coverKey>"],
    ["t", "napstr-cover"]
  ],
  "content": "{\"protocol\":\"napstr/1\",\"deleted\":true}"
}
```

Consumers MUST treat the latest event at that coordinate from that author as
withdrawn and MUST NOT offer the author's older claim.

## Validation requirements

An interoperable consumer MUST:

- validate the event ID and signature per NIP-01;
- require `protocol` = `napstr/1`, the `napstr-cover` marker tag, and a cover
  key containing exactly one `|` with non-empty, length-bounded halves;
- require `art` to be an HTTPS URL or absent in favour of `x`;
- require `x`, when present, to be a 64-character lowercase hex SHA-256;
- treat `year`, `genre`, `collection`, and `source` as untrusted display text;
- verify embedded-cover bytes by SHA-256 and image magic bytes after transfer.

A publisher MUST NOT include local filesystem paths, private keys, or
transfer capabilities anywhere in the event.

A publisher SHOULD refuse to publish covers for tracks whose embedded tags are
obviously junk: URL or watermark prefixes, bare filenames as album names, or
leading track numbers. A bad cover on the network is worse than no cover.

## Neighbourhood occupancy (non-normative)

A probe of kinds `30415`–`30435` on 2026-09-21 found no empty coordinate from
`30420` upward. Sampled events and their occupants:

| kind | occupants | notes |
| --- | --- | --- |
| `30420` | text-adventure game, encrypted session app, lobby app | game went quiet 2026-05-18; others active to 2026-09-18 |
| `30421` | Napstr catalogue (961), game shop listings, LLM oracle app | Napstr's own kind, shared |
| `30422` | art marketplace (≈350), Napstr availability (47), game state | far more foreign than Napstr |
| `30423` | Napstr audiobooks (59), art gallery (85), game loot (9) | three unrelated apps at one coordinate |
| `30424` | encrypted session app, text-adventure game | game stopped 2026-05-05, app resumed 2026-09-18 |
| `30425` | encrypted session app (42), text-adventure game (28), `napstr-playlist` (1) | quiet 2026-05-12 → 2026-09-15, then resumed |
| `30426` | text-adventure game, encrypted session app | game stopped 2026-05-01 |
| `30427` | Napstr covers (712), encrypted session app (3) | **this kind** |
| `30428`, `30429` | text-adventure game trades | `30429` untouched since 2026-04-29 |
| `30430` | `voidstrike` / `macsand` lobby codes (514) | one event per author |

`30418` and `30419` are the only probed coordinates with zero events. Kind
selection is therefore best-effort and time-bound: a coordinate that is empty
today may be occupied next month, which is why the marker tag carries the
disambiguation rather than the kind number.

The most consequential occupant in the table is the unidentified "encrypted
session app". It is a peer-addressed protocol: exactly two tags, `p` and `d`,
where `d` is either `<8-char base36>-<8-char base36>:<peer pubkey>` or a bare
`<peer pubkey>`, and `p` repeats that same key. Every payload is NIP-44 version 2
ciphertext, so no string in it is searchable and no identity is recoverable. It
used 80 freshly generated keys across 76 sessions, entirely between 2026-09-12
and 2026-09-18, and its events exist on only two of eighteen relays probed
(`relay.damus.io` and `nos.lol`). Those keys publish nothing else at all — no
kind `0`, no kind `1`, no relay list.

Its two busiest channels are `30422` (84 sampled events) and `30423` (53) — the
Napstr availability and audiobook kinds — so a client that read either kind
without checking the marker would ingest encrypted blobs as heartbeats or
manifests. `merge_availability_events` requires the `napstr-availability` tag and
`valid_audiobook_event` requires `napstr-audiobook`, and that requirement is the
only reason this is harmless.

## Kind coexistence (non-normative)

NIP-01 defines the kind ranges as conventions — "just conventions and relay
implementations may differ" — and states that "each NIP may define the meaning of
a set of kinds that weren't defined elsewhere". It provides no allocation
authority, no reservation mechanism, and no exclusivity: any keypair may publish
any kind at any time, and collisions are expected rather than exceptional.
Occupying this range is therefore ordinary practice, as is the fact that others
already occupy it.

What follows from that is not a permission question but three obligations:

- **Clients MUST filter by marker tag and MUST NOT rely on kind exclusivity.**
  `{"kinds":[30427]}` alone ingests foreign events.
- **Publishers MUST NOT assume a coordinate is theirs alone, and SHOULD choose
  `d` values whose *shape* is distinct from neighbours' patterns.** A cover key
  (`artist|album`) and a playlist ID (a UUID) are both distinct in shape from the
  neighbours' `d` patterns observed here, which is what keeps shape collisions
  from becoming bugs. Napstr's catalogue and audiobook `d` values are 64-character
  hex, which is **shape-identical to a public key** — the same shape the session
  app uses for its peer-addressed slots. On `30421` and `30423` the marker tag is
  the *only* thing separating a Napstr file ID from a peer key, so a `#d` query
  against those kinds is ambiguous by construction and MUST be accompanied by the
  marker.
- **Napstr events MUST stay ignorable by any foreign client.** The markers and the
  `d` shapes achieve this. Note that the unidentified protocol publishes bare `p`
  and `d` with no marker whatsoever, which makes it the fragile participant in
  this neighbourhood rather than Napstr: it has no way to exclude our events from
  its own queries, whereas we have a marker on every event.

## Rationale notes (non-normative)

- **Why a new kind instead of an `image` tag on `30421`:** a cover belongs to
  an *album*, not a file; duplicating the URL across every track event bloats
  the catalogue, and addressability per album key gives clean replace/delete
  semantics.
- **Why publishers and not just curators:** seeders are the only parties with
  provable access to the actual audio (and its embedded art), which is also
  the strongest trust signal available without a web-of-trust layer.
- **Why keep the messy edition markers in the key:** matching is against
  catalogue display strings; canonicalization belongs in the optional `mbid` /
  `collection` fields, where getting it wrong costs nothing.
