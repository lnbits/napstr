import test from 'node:test';
import assert from 'node:assert/strict';
import { parseDeepLink } from '../android/src/lib/deepLink.ts';

const ID = 'a'.repeat(64);

test('a track link needs a real SHA-256 and comes back lower-cased', () => {
  assert.deepEqual(parseDeepLink(`napstrfy://track/${ID}`), { kind: 'track', fileId: ID });
  assert.deepEqual(parseDeepLink(`napstrfy://track/${'A'.repeat(64)}`), { kind: 'track', fileId: ID });
  assert.deepEqual(parseDeepLink(`NAPSTRFY://TRACK/${ID}`), { kind: 'track', fileId: ID });
  assert.deepEqual(parseDeepLink(`  napstrfy://track/${ID}\n `), { kind: 'track', fileId: ID });
});

test('a track link that is not a SHA-256 is not a track to look for', () => {
  const rejected = [
    '',
    'napstrfy://track',
    'napstrfy://track/',
    `napstrfy://track/${'a'.repeat(63)}`,
    `napstrfy://track/${'a'.repeat(65)}`,
    `napstrfy://track/${'z'.repeat(64)}`,
    `napstrfy://track/${ID}/extra`,
    `napstr://track/${ID}`,
    `https://napstr.net/track/${ID}`,
    `napstrfy:/track/${ID}`
  ];
  for (const value of rejected) assert.equal(parseDeepLink(value), null, value);
});

test('a pairing link is handed over whole, because the ticket is the link', () => {
  const ticket = `napstrfy://pair/${'B'.repeat(43)}`;
  assert.deepEqual(parseDeepLink(ticket), { kind: 'pair', ticket });
  // The ticket is base64url, so it must survive untouched rather than decoded.
  assert.deepEqual(parseDeepLink('napstrfy://pair/ab-_cd').ticket, 'napstrfy://pair/ab-_cd');
  assert.equal(parseDeepLink('napstrfy://pair/'), null);
  assert.equal(parseDeepLink('napstrfy://pair'), null);
});

test('an album link carries search terms, however they were escaped', () => {
  assert.deepEqual(parseDeepLink('napstrfy://album/Indestructible'), { kind: 'album', terms: 'Indestructible' });
  assert.deepEqual(parseDeepLink('napstrfy://album/Rancid%20-%20Indestructible'), { kind: 'album', terms: 'Rancid - Indestructible' });
  assert.deepEqual(parseDeepLink('napstrfy://album/Rancid+-+Indestructible'), { kind: 'album', terms: 'Rancid - Indestructible' });
  assert.deepEqual(parseDeepLink('napstrfy://album/  spaced   out  '), { kind: 'album', terms: 'spaced out' });
  // A malformed escape still searches the text as it arrived.
  assert.deepEqual(parseDeepLink('napstrfy://album/100%'), { kind: 'album', terms: '100%' });
  assert.equal(parseDeepLink('napstrfy://album/'), null);
  assert.equal(parseDeepLink('napstrfy://album/%20'), null);
});

test('anything else is not ours to open', () => {
  for (const other of ['napstrfy://', 'napstrfy://playlist/1', 'napstrfy:///track', 'plain text', 'napstrfy:']) {
    assert.equal(parseDeepLink(other), null, other);
  }
});
