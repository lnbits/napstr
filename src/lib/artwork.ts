import { invoke } from '@tauri-apps/api/core';

/**
 * Album art for the desktop window.
 *
 * The backend owns every trust decision here: which kind `30427` claim wins (a
 * seeder's beats a stranger's, and inside a class the newest wins), and whether
 * this computer has art of its own that no relay has ever seen. This module
 * only batches the questions, so a page of results costs one call rather than
 * one per row, and remembers the answers for the session.
 */

/** Mirrors `cover::AlbumCover` on the host side. */
export type AlbumCover = {
  /** The verbatim `artist|album` key this cover answers. */
  key: string;
  /** Front cover URL. Empty when the publisher only shared an embedded copy. */
  art: string;
  /** Smaller rendition of the same image, when the publisher supplied one. */
  thumb: string;
  mbid: string;
  year: string;
  genre: string;
  collection: string;
  /** Provenance hint: `itunes`, `musicbrainz`, `embedded`, `manual`. */
  source: string;
  coverFileId: string;
  mime: string;
  /** Author of the winning claim. Empty when only this computer resolved it. */
  author: string;
  /** Id of the winning kind `30427`. Empty for a local resolution. */
  eventId: string;
  createdAt: number;
  seeder: boolean;
};

/** Keys per call, matching what the host's own relay batching can absorb. */
const INVOKE_KEY_LIMIT = 160;
/** A results page composes in stages: gather its keys briefly before asking. */
const BATCH_DELAY_MS = 40;

type Waiter = (cover: AlbumCover | null) => void;

/**
 * Covers resolved while the window is open, including albums with none.
 *
 * This is deliberately session-only. The host already caches relay answers for
 * hours on its own schedule, so persisting here would buy one saved local call
 * in exchange for showing art the host would have replaced.
 */
const sessionCovers = new Map<string, AlbumCover | null>();
const pending = new Map<string, Waiter[]>();
let flushHandle: number | null = null;

/**
 * The cover key from the NIP: `trim(artist)|trim(album)`, lowercased, with no
 * other normalization so it matches catalogue display strings byte for byte.
 *
 * This duplicates `cover::cover_key` in Rust for the same reason the phone
 * duplicates it: those are two languages with no shared code between them.
 * Returns `''` when the pair cannot be addressed.
 */
export function coverKey(artist: string, album: string): string {
  const left = (artist ?? '').trim().toLowerCase();
  const right = (album ?? '').trim().toLowerCase();
  if (!left || !right) return '';
  if (left.includes('|') || right.includes('|')) return '';
  const key = `${left}|${right}`;
  return key.length <= 300 ? key : '';
}

/** True when this art came from somebody's signed kind `30427` claim. */
export function isPublished(cover: AlbumCover): boolean {
  return Boolean(cover.author);
}

/** The URL to draw: `thumb` in dense grids, `art` where there is room. */
export function artUrl(cover: AlbumCover | null, preferThumb: boolean): string {
  if (!cover) return '';
  return preferThumb ? cover.thumb || cover.art : cover.art || cover.thumb;
}

/** What a window reports to the backend as the results pane draws a page. */
export type CoverAlbum = { artist: string; album: string };

/**
 * The NIP-56 reasons a cover report may use.
 *
 * The values must match `REPORT_REASONS` in `remote-protocol`, and the labels
 * are the same words the phone offers, so both apps describe the same report
 * the same way. `impersonation` is glossed as wrong artist or album because
 * that is what a person reporting a cover actually means by it.
 */
export const coverReportReasons = [
  { value: 'spam', label: 'Spam or advertising' },
  { value: 'illegal', label: 'Illegal content' },
  { value: 'malware', label: 'Malware or a scam' },
  { value: 'impersonation', label: 'Wrong artist or album' },
  { value: 'nudity', label: 'Nudity' },
  { value: 'profanity', label: 'Profanity' },
  { value: 'other', label: 'Something else' }
] as const;

export type CoverReportReason = (typeof coverReportReasons)[number]['value'];

/** The protocol's own list of reasons, for a caller that has no labels. */
export const coverReportReasonValues = coverReportReasons.map((reason) => reason.value);

/**
 * The distinct, addressable albums a list of results names.
 *
 * Only the rendered page is reported, which is what "albums seen in search
 * results" means — reporting the whole catalogue cache instead is thousands of
 * albums nobody looked at. Key computation stays in Rust, so this sends display
 * metadata rather than a key the frontend would have to normalize identically.
 */
export function albumsFor(
  items: { artist?: string; album?: string }[]
): CoverAlbum[] {
  const seen = new Set<string>();
  const albums: CoverAlbum[] = [];
  for (const item of items) {
    const artist = (item.artist ?? '').trim();
    const album = (item.album ?? '').trim();
    const key = coverKey(artist, album);
    if (!key || seen.has(key)) continue;
    seen.add(key);
    albums.push({ artist, album });
  }
  return albums;
}

/**
 * The cover for one album, batched with every other request made in the same
 * tick. Resolves to `null` when nobody has a cover and this computer has none.
 */
export function coverFor(artist: string, album: string): Promise<AlbumCover | null> {
  const key = coverKey(artist, album);
  if (!key) return Promise.resolve(null);
  const known = sessionCovers.get(key);
  if (known !== undefined) return Promise.resolve(known);
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
    if (!unanswered.has(key)) sessionCovers.set(key, cover);
    for (const resolve of waiters) resolve(cover);
  }
  // Keys requested while this batch was in flight wait for the next one.
  if (pending.size) flushHandle = window.setTimeout(flush, BATCH_DELAY_MS);
}

async function resolveCovers(
  keys: string[]
): Promise<{ covers: Map<string, AlbumCover>; unanswered: Set<string> }> {
  const covers = new Map<string, AlbumCover>();
  const unanswered = new Set<string>();
  for (let index = 0; index < keys.length; index += INVOKE_KEY_LIMIT) {
    const slice = keys.slice(index, index + INVOKE_KEY_LIMIT);
    try {
      const found = await invoke<AlbumCover[]>('cover_art', { keys: slice });
      for (const cover of found) {
        if (cover.art || cover.thumb || cover.coverFileId) covers.set(cover.key, cover);
      }
    } catch {
      // Offline, or a host that cannot answer right now. Nothing was learned.
      for (const key of slice) unanswered.add(key);
    }
  }
  return { covers, unanswered };
}

/**
 * Forget every session answer.
 *
 * Called when a cover scan finishes: the scan is exactly the thing that turns
 * "nobody has art for this album" into art, so holding the old answer would
 * hide the result the user just waited for. Anything already waiting is
 * answered rather than abandoned, so no row is left hanging forever.
 */
export function clearCoverCache() {
  sessionCovers.clear();
  if (flushHandle !== null) {
    window.clearTimeout(flushHandle);
    flushHandle = null;
  }
  const waiting = [...pending.values()].flat();
  pending.clear();
  for (const resolve of waiting) resolve(null);
}
