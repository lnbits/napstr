import { invoke } from '@tauri-apps/api/core';
import { writable } from 'svelte/store';
import { recordCoverEvent } from './coverDebug';
import type { RemoteTrack } from './types';

/**
 * Album artwork asserted by Napstr kind `30427` cover events.
 *
 * The paired host resolves these, because it owns the relay pool, the
 * catalogue, and the availability heartbeats that decide which claim wins.
 * Lookups are batched: a list page asks for every album it shows in one call
 * instead of querying per row. An album the host has no cover event for simply
 * has no artwork here.
 */
export type AlbumCover = {
  key: string;
  /** Front cover URL. Empty when the publisher only shared an embedded copy. */
  art: string;
  /** Smaller rendition of the same image, when the publisher supplied one. */
  thumb: string;
  mbid: string;
  year: string;
  genre: string;
  collection: string;
  /** Provenance hint from the publisher, such as `itunes`, `musicbrainz`, or `embedded`. */
  source: string;
  coverFileId: string;
  mime: string;
  author: string;
  seeder: boolean;
};

const CACHE_PREFIX = 'napstrfy-cover:v2:';
/** The pre-cover-event namespace, swept once so it cannot linger forever. */
const LEGACY_CACHE_PREFIX = 'napstrfy-artwork:';
/** Keys per companion call. Mirrors `MAX_COVER_KEYS * 4` in the phone crate. */
const INVOKE_KEY_LIMIT = 160;
/**
 * How long stored artwork is trusted before it is asked for again.
 *
 * Publishers replace their cover claim, and image URLs rot, so a cover that
 * answered once is not permanent. Asking is a local call to the host.
 */
const STORED_COVER_TTL_MS = 7 * 24 * 60 * 60 * 1000;
/**
 * How long "the host has no cover for this" is trusted on its own.
 *
 * The host reports a revision whenever its art changes, which is the accurate
 * signal; this is what still works against a host too old to report one.
 */
const NEGATIVE_TTL_MS = 60 * 1000;
/**
 * How long a tile waits before asking again after this album had nothing to
 * draw, or the image the host named would not load.
 */
export const ARTWORK_RETRY_MS = 60 * 1000;
/** A page composes in stages: gather its keys briefly before asking. */
const BATCH_DELAY_MS = 40;

/**
 * Bumped when there is a cached "no cover" worth asking about again, so
 * artwork already on screen asks instead of waiting for a re-render. Read as
 * `$coverRevision` by the components that draw art.
 */
export const coverRevision = writable(0);
/** The host revision this phone has already acted on. */
let appliedCoverRevision = 0;

type Waiter = (cover: AlbumCover | null) => void;

/**
 * Covers resolved while the app is open, including albums the host has no cover
 * for. Absence is deliberately not persisted: the host re-checks relays on its
 * own schedule and answering again costs one batched call, so carrying a "no"
 * across launches only ever delays artwork that has since been published.
 */
const sessionCovers = new Map<string, AlbumCover | null>();
/**
 * When the host last said it had no cover for a key. Absence is not permanent —
 * the host is looking albums up as they are shown here — so a "no" that has
 * stood for `NEGATIVE_TTL_MS` is asked about again rather than kept.
 */
const negativeAt = new Map<string, number>();
const pending = new Map<string, Waiter[]>();
let flushHandle: number | null = null;

/**
 * `undefined` means nothing is known yet; `null` means the host already answered
 * and has no cover for this album. The two must stay distinct: treating
 * "unknown" as "no cover" resolves the answer without ever asking for it.
 */
function cachedCover(key: string): AlbumCover | null | undefined {
  const known = sessionCovers.get(key);
  if (known !== undefined) {
    if (known !== null || !expiredNegative(key)) return known;
    // Falling through asks again, which is the point of an expiry.
  }
  const stored = readStoredCover(key);
  if (stored === undefined) return undefined;
  sessionCovers.set(key, stored);
  return stored;
}

/** Forget a "the host has none" that has been trusted for long enough. */
function expiredNegative(key: string): boolean {
  const asked = negativeAt.get(key) ?? 0;
  if (Date.now() - asked <= NEGATIVE_TTL_MS) return false;
  sessionCovers.delete(key);
  negativeAt.delete(key);
  return true;
}

function readStoredCover(key: string): AlbumCover | undefined {
  try {
    const raw = window.localStorage.getItem(CACHE_PREFIX + key);
    if (!raw) return undefined;
    const stored = JSON.parse(raw) as { at: number; cover: AlbumCover | null };
    if (!stored.cover) {
      // An earlier build could store a "no cover" verdict. Drop it rather than
      // let a stale no keep suppressing the question.
      dropStoredCover(key);
      return undefined;
    }
    if (Date.now() - stored.at > STORED_COVER_TTL_MS) return undefined;
    return stored.cover;
  } catch {
    return undefined;
  }
}

/** Remember what a request learned. Only artwork is worth writing to disk. */
function rememberCover(key: string, cover: AlbumCover | null) {
  sessionCovers.set(key, cover);
  if (!cover) {
    negativeAt.set(key, Date.now());
    // A claim can be withdrawn, so a stored cover must go when the host says
    // there is none rather than linger until its own expiry.
    dropStoredCover(key);
    return;
  }
  negativeAt.delete(key);
  try {
    window.localStorage.setItem(CACHE_PREFIX + key, JSON.stringify({ at: Date.now(), cover }));
  } catch {
    // Artwork is cosmetic; a full or unavailable cache must not affect playback.
  }
}

function dropStoredCover(key: string) {
  try {
    window.localStorage.removeItem(CACHE_PREFIX + key);
  } catch {
    // Nothing to reclaim; the entry is only wasted space.
  }
}

function dropLegacyCache() {
  try {
    const stale: string[] = [];
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const name = window.localStorage.key(index);
      if (name?.startsWith(LEGACY_CACHE_PREFIX)) stale.push(name);
    }
    for (const name of stale) window.localStorage.removeItem(name);
  } catch {
    // A cache that cannot be swept is only wasted space.
  }
}

dropLegacyCache();

/**
 * The cover key from the cover NIP: `trim(artist)|trim(album)`, lowercased,
 * preserved verbatim otherwise so it matches the catalogue display strings.
 * Exported because album grouping in the UI must use the same identity.
 */
export function coverKey(artist: string, album: string): string {
  const artistHalf = artist.trim().toLowerCase();
  const albumHalf = album.trim().toLowerCase();
  if (!artistHalf || !albumHalf) return '';
  if (artistHalf.includes('|') || albumHalf.includes('|')) return '';
  const key = `${artistHalf}|${albumHalf}`;
  return key.length <= 300 ? key : '';
}

export function coverFor(track: RemoteTrack): Promise<AlbumCover | null> {
  const key = coverKey(track.artist ?? '', track.album ?? '');
  if (!key) {
    recordCoverEvent('skip', `${track.title || track.filename}: no artist or album tag`);
    return Promise.resolve(null);
  }
  return requestCover(key);
}

/** Renditions already asked for, by URL, with the load that was started. */
const renditionLoads = new Map<string, Promise<string>>();

/**
 * Fetch a rendition now and hand back what landed: its URL, or '' when the image
 * would not load.
 *
 * One fetch per URL however many callers ask, so a view that draws the same
 * cover, and a preload that has already fetched it, cost one download between
 * them. A failure is remembered for the session, which is what keeps an album
 * whose image 404s from being retried on every track.
 */
function loadRendition(url: string): Promise<string> {
  if (!url) return Promise.resolve('');
  const asked = renditionLoads.get(url);
  if (asked) return asked;
  const landed = new Promise<string>((resolve) => {
    // Nothing holds this element: the fetch it starts is the point, and whether
    // the cover is still wanted when it lands is decided by the screen itself.
    const image = new Image();
    image.decoding = 'async';
    image.onload = () => resolve(url);
    image.onerror = () => resolve('');
    image.src = url;
  });
  renditionLoads.set(url, landed);
  return landed;
}

/**
 * Fetch both renditions of a track's cover now, rather than when the screens that
 * show them appear. This is for the track coming next.
 *
 * By the time it plays, its cover has been asked about, its thumbnail is in the
 * image cache - so the player is a cover from its first frame - and its full
 * cover has landed, so the drawer fades it in without waiting and the lock screen
 * is able to move on to it at once.
 *
 * Fetching the full rendition is a deliberate cost, and one worth revisiting on a
 * metered connection: every track that becomes the next one has its full cover
 * downloaded whether or not anything gets to draw it. Dropping the second call
 * below would leave everything else as it is, with the lock screen upgrading when
 * its own fetch lands rather than before the track is even played.
 */
export function preloadArtwork(track: RemoteTrack) {
  void coverFor(track).then((cover) => {
    if (!cover) return;
    void loadRendition(cover.thumb);
    void loadRendition(cover.art);
  });
}

/**
 * The full cover of an album, fetched now and resolved when it has landed. The
 * URL is handed back rather than a flag so a caller can publish the very image it
 * waited for, and '' means the publisher's rendition would not load at all.
 */
export function loadFullCover(cover: AlbumCover | null): Promise<string> {
  return loadRendition(cover?.art ?? '');
}

/**
 * Forget one album's cover because the image itself would not load, so the
 * next ask resolves a fresh URL instead of handing back the dead one. The
 * stored copy goes too: it names the same URL.
 */
export function dropCover(track: RemoteTrack) {
  const key = coverKey(track.artist ?? '', track.album ?? '');
  if (!key) return;
  sessionCovers.delete(key);
  negativeAt.delete(key);
  dropStoredCover(key);
}

function requestCover(key: string): Promise<AlbumCover | null> {
  const cached = cachedCover(key);
  if (cached !== undefined) {
    recordCoverEvent('cached', `${key} → ${cached ? 'cover' : 'host has none'}`);
    return Promise.resolve(cached);
  }
  return new Promise((resolve) => {
    const waiters = pending.get(key);
    if (waiters) waiters.push(resolve);
    else pending.set(key, [resolve]);
    if (flushHandle === null) flushHandle = window.setTimeout(flush, BATCH_DELAY_MS);
  });
}

async function flush() {
  flushHandle = null;
  const batch = [...pending.keys()];
  const { covers, unanswered } = await resolveCovers(batch);
  for (const key of batch) {
    const waiters = pending.get(key) ?? [];
    pending.delete(key);
    const cover = covers.get(key) ?? null;
    // A request that failed is not an answer, so it must not be remembered as
    // "this album has no cover". Only the host saying so is remembered.
    if (!unanswered.has(key)) rememberCover(key, cover);
    for (const resolve of waiters) resolve(cover);
  }
  // Keys requested while this batch was in flight wait for the next one.
  if (pending.size) flushHandle = window.setTimeout(flush, BATCH_DELAY_MS);
}

type CoverResolution = {
  /** Covers the host answered with, keyed by cover key. */
  covers: Map<string, AlbumCover>;
  /** Keys whose request failed, so nothing was learned about them. */
  unanswered: Set<string>;
};

async function resolveCovers(keys: string[]): Promise<CoverResolution> {
  const covers = new Map<string, AlbumCover>();
  const unanswered = new Set<string>();
  for (let index = 0; index < keys.length; index += INVOKE_KEY_LIMIT) {
    const slice = keys.slice(index, index + INVOKE_KEY_LIMIT);
    const started = Date.now();
    recordCoverEvent('request', `${slice.length} keys: ${previewKeys(slice)}`);
    try {
      const found = await invoke<AlbumCover[]>('remote_covers', { keys: slice });
      recordCoverEvent(
        'answer',
        `${slice.length} keys → ${found.length} covers in ${Date.now() - started} ms`
      );
      for (const cover of found) {
        if (cover.art || cover.thumb || cover.coverFileId) covers.set(cover.key, cover);
      }
    } catch (error) {
      // Unpaired, offline, or a host older than the cover NIP.
      recordCoverEvent(
        'error',
        `${slice.length} keys failed in ${Date.now() - started} ms: ${String(error)}`
      );
      for (const key of slice) unanswered.add(key);
    }
  }
  return { covers, unanswered };
}

function previewKeys(keys: string[]): string {
  const shown = keys.slice(0, 2).join(', ');
  return keys.length > 2 ? `${shown}, +${keys.length - 2}` : shown;
}

/** Temporary: what the cache currently knows, for the debug panel. */
export function debugCoverState(tracks: RemoteTrack[]) {
  return tracks.map((track) => {
    const key = coverKey(track.artist ?? '', track.album ?? '');
    const known = key ? sessionCovers.get(key) : undefined;
    const stored = key ? readStoredCover(key) : undefined;
    const cover = known ?? stored ?? null;
    return {
      fileId: track.fileId,
      title: track.title || track.filename,
      key,
      state: !key
        ? 'no key'
        : known === null
          ? 'host: none'
          : cover
            ? 'resolved'
            : 'unknown',
      url: cover ? cover.thumb || cover.art : ''
    };
  });
}

/**
 * Ask again about every album the host said it had no art for, once the host's
 * own art has changed.
 *
 * Art already drawn is left alone: replacing a picture that is on screen costs
 * a flash of the fallback for no gain, and a claim that supersedes another is
 * rare enough to wait for the stored copy to age out. The case worth acting on
 * at once is the album with nothing at all, because this phone asked before the
 * host knew — which is exactly the art that turns up a moment later.
 */
export function invalidateCoverNegatives(revision: number) {
  if (revision === appliedCoverRevision) return;
  appliedCoverRevision = revision;
  let dropped = 0;
  for (const [key, cover] of sessionCovers) {
    if (cover !== null) continue;
    sessionCovers.delete(key);
    negativeAt.delete(key);
    dropped += 1;
  }
  // Nothing was known to be missing, so nothing on screen needs to move.
  if (dropped === 0) return;
  recordCoverEvent('refresh', `host art changed at revision ${revision}; re-asking ${dropped}`);
  coverRevision.update((value) => value + 1);
}

/** Temporary: drop every cached cover so the next render asks again. */
export function clearCoverCache() {
  sessionCovers.clear();
  negativeAt.clear();
  pending.clear();
  coverRevision.update((value) => value + 1);
  try {
    const stale: string[] = [];
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const name = window.localStorage.key(index);
      if (name?.startsWith(CACHE_PREFIX)) stale.push(name);
    }
    for (const name of stale) window.localStorage.removeItem(name);
  } catch {
    // Nothing to clear.
  }
}

export function artworkHue(fileId: string) {
  return Number.parseInt(fileId.slice(0, 6) || '5632aa', 16) % 360;
}
