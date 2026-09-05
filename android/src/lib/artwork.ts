import type { RemoteTrack } from './types';

const CACHE_PREFIX = 'napstrfy-artwork:';
let lookupQueue = Promise.resolve();
let nextLookup = 0;

function readCache(key: string) {
  try {
    return window.localStorage.getItem(CACHE_PREFIX + key);
  } catch {
    return null;
  }
}

function writeCache(key: string, value: string) {
  try {
    window.localStorage.setItem(CACHE_PREFIX + key, value);
  } catch {
    // Artwork is cosmetic; a full or unavailable cache must not affect playback.
  }
}

function pause(ms: number) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

export function artworkFor(track: RemoteTrack): Promise<string> {
  if (!track.artist || !track.album) return Promise.resolve('');
  const key = `${track.artist.toLocaleLowerCase()}\u0000${track.album.toLocaleLowerCase()}`;
  const cached = readCache(key);
  if (cached !== null) return Promise.resolve(cached);
  const work = lookupQueue.then(async () => {
    const wait = Math.max(0, nextLookup - Date.now());
    if (wait) await pause(wait);
    nextLookup = Date.now() + 1100;
    try {
      const query = `release:${JSON.stringify(track.album)} AND artist:${JSON.stringify(track.artist)}`;
      const response = await fetch(`https://musicbrainz.org/ws/2/release/?fmt=json&limit=1&query=${encodeURIComponent(query)}`);
      if (!response.ok) throw new Error('artwork lookup failed');
      const data = await response.json() as { releases?: { id?: string }[] };
      const candidate = data.releases?.[0]?.id ?? '';
      const release = /^[0-9a-f]{8}-[0-9a-f-]{27}$/i.test(candidate) ? candidate : '';
      const url = release ? `https://coverartarchive.org/release/${release}/front-250` : '';
      writeCache(key, url);
      return url;
    } catch {
      writeCache(key, '');
      return '';
    }
  });
  lookupQueue = work.then(() => undefined, () => undefined);
  return work;
}

export function artworkHue(fileId: string) {
  return Number.parseInt(fileId.slice(0, 6) || '5632aa', 16) % 360;
}
