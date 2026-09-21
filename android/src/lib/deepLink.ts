/**
 * The links the companion answers to:
 *
 *   napstrfy://track/<sha256>   a track this phone, or its computer, already has
 *   napstrfy://pair/<ticket>    pair with a computer over Iroh
 *   napstrfy://album/<terms>    look an album up
 *
 * Parsing is kept apart from the app so the accepted forms can be tested without
 * a device or a window, and it is strict on purpose: a track id is a SHA-256, so
 * anything else in that position is somebody else's link rather than a track to
 * go looking for.
 */
export type DeepLink =
  | { kind: 'track'; fileId: string }
  | { kind: 'pair'; ticket: string }
  | { kind: 'album'; terms: string };

/** The scheme the companion registers with the operating system. */
export const DEEP_LINK_SCHEME = 'napstrfy';

const TRACK_ID = /^[0-9a-f]{64}$/;

export function parseDeepLink(value: string): DeepLink | null {
  const trimmed = value.trim();
  const prefix = `${DEEP_LINK_SCHEME}://`;
  if (trimmed.slice(0, prefix.length).toLowerCase() !== prefix) return null;
  const rest = trimmed.slice(prefix.length);
  const slash = rest.indexOf('/');
  if (slash < 0) return null;
  const host = rest.slice(0, slash).toLowerCase();
  const tail = rest.slice(slash + 1);

  if (host === 'track') {
    // Ids are written lower-case everywhere else in this app, and a link that
    // arrives upper-case is the same track rather than a different one.
    const fileId = tail.trim().toLowerCase();
    return TRACK_ID.test(fileId) ? { kind: 'track', fileId } : null;
  }

  if (host === 'pair') {
    // A pairing ticket is itself a `napstrfy://pair/...` URI, so the link and
    // the ticket are the same string: hand it over whole and let the pairing
    // code decide whether it is a valid one. Decoding it would corrupt the
    // base64url it is made of.
    return tail.trim() ? { kind: 'pair', ticket: trimmed } : null;
  }

  if (host === 'album') {
    let terms = tail;
    try {
      terms = decodeURIComponent(terms);
    } catch {
      // A malformed escape is not worth refusing the link over: search the text
      // as it arrived.
    }
    terms = terms.replace(/\+/g, ' ').replace(/\s+/g, ' ').trim();
    return terms ? { kind: 'album', terms } : null;
  }

  return null;
}
