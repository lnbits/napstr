//! Consumer for the Napstr album cover event (kind `30427`).
//!
//! The wire format is specified by `NIP-NAPSTR-COVER.md`. Cover events are
//! additive assertions about an album's artwork, addressed by a normalized
//! `artist|album` key, so every path in this module degrades to "no cover
//! known" instead of failing a catalogue read. Covers never alter, suppress, or
//! reclassify kind `30421` entries.
//!
//! Two trust rules from the NIP shape the ranking here:
//!
//! 1. A claim whose author is a currently active seeder of at least one track
//!    of that album outranks claims from everybody else.
//! 2. Inside the same trust class the newest `created_at` wins.
//!
//! Events from blocked authors are dropped before ranking, and a withdrawn
//! coordinate (`{"deleted":true}`) is stored as a tombstone so an older claim
//! from the same author can never be offered.

use chrono::Utc;
use nostr_sdk::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const COVER_KIND: u16 = 30427;
pub const COVER_MARKER: &str = "napstr-cover";
/// `content` is JSON with camel-case keys, at most 4 KiB.
pub const COVER_CONTENT_BYTE_LIMIT: usize = 4 * 1024;
/// A `d` value is `artist|album` and must not exceed 300 characters.
pub const COVER_KEY_CHARACTER_LIMIT: usize = 300;
/// Untrusted display text (`year`, `genre`, `collection`, `source`) is bounded.
const COVER_TEXT_CHARACTER_LIMIT: usize = 120;
const COVER_ART_URL_CHARACTER_LIMIT: usize = 2_048;
const COVER_MIME_CHARACTER_LIMIT: usize = 64;
const COVER_KEY_SEPARATOR: char = '|';

/// The raw JSON body of a kind `30427` event. Unknown properties are ignored,
/// as the NIP requires.
///
/// `Serialize` is derived for the publishing side (`cover_event`), so the body
/// this host writes is produced by the same declaration that reads it and the
/// two cannot drift apart.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverContent {
    protocol: String,
    #[serde(default)]
    art: String,
    #[serde(default)]
    mbid: String,
    #[serde(default)]
    year: String,
    #[serde(default)]
    genre: String,
    #[serde(default)]
    collection: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    deleted: bool,
}

/// A winning cover for one album key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumCover {
    /// The verbatim cover key this cover answers, even when it was matched
    /// through the canonical alias.
    pub key: String,
    /// HTTPS URL of the front cover. Empty when the publisher only shared an
    /// embedded copy through the `x` tag.
    pub art: String,
    /// HTTPS URL of a smaller rendition of the same image, when published.
    pub thumb: String,
    /// MusicBrainz release-group MBID the art was resolved from, when known.
    pub mbid: String,
    pub year: String,
    pub genre: String,
    /// Canonical release title, when it differs from the catalogue `album`.
    pub collection: String,
    /// Provenance hint. Informational only; never a trust signal.
    pub source: String,
    /// Lowercase SHA-256 of an embedded cover image published as a kind `30421`
    /// entry, when the publisher shared the bytes instead of only a URL.
    pub cover_file_id: String,
    pub mime: String,
    /// Author of the winning claim.
    pub author: String,
    pub event_id: String,
    pub created_at: u64,
    /// True when the author is (or was) an active seeder of a track of this
    /// album, which makes the claim win over non-seeder claims.
    pub seeder: bool,
}

/// The outcome of reading one kind `30427` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverClaim {
    /// A usable cover assertion.
    Art(Box<AlbumCover>),
    /// The coordinate was replaced with the withdrawal body. Consumers must not
    /// offer this author's older claim.
    Withdrawn {
        key: String,
        author: String,
        event_id: String,
        created_at: u64,
    },
}

// ---------------------------------------------------------------------------
// Cover keys
// ---------------------------------------------------------------------------

/// `trim(artist) + "|" + trim(album)`, Unicode-case-folded.
///
/// The key exists to match catalogue display strings byte-for-byte, so no other
/// normalization happens: whitespace runs, diacritics, punctuation, and edition
/// markers are preserved. Returns `None` when either half is empty, when the
/// halves would not round-trip through a single separator, or when the result
/// exceeds the NIP's length bound.
pub fn cover_key(artist: &str, album: &str) -> Option<String> {
    normalise_cover_key(&format!(
        "{artist}{COVER_KEY_SEPARATOR}{album}"
    ))
}

/// Re-normalize a key that came from a caller or from an event tag.
///
/// A key is valid when it holds exactly one separator with non-empty,
/// length-bounded halves.
pub fn normalise_cover_key(value: &str) -> Option<String> {
    let mut halves = value.split(COVER_KEY_SEPARATOR);
    let artist = halves.next()?.trim().to_lowercase();
    let album = halves.next()?.trim().to_lowercase();
    if halves.next().is_some() || artist.is_empty() || album.is_empty() {
        return None;
    }
    let key = format!("{artist}{COVER_KEY_SEPARATOR}{album}");
    (key.chars().count() <= COVER_KEY_CHARACTER_LIMIT).then_some(key)
}

/// `cover_key` recomputed from cleaned display metadata.
///
/// Mis-tagged catalogues are common: watermarks, filenames used as album names,
/// and edition markers. Publishers share the cleaned key as `c=<canonicalKey>`
/// so those entries can still match. This is a heuristic on the NIP's "etc.";
/// it only ever widens lookup, it is never used to publish a `d` value.
pub fn canonical_cover_key(artist: &str, album: &str) -> Option<String> {
    cover_key(&clean_artist(artist), &clean_album(album))
}

/// Both `#d` selectors that can answer `key`: the verbatim key first, then the
/// canonical alias when it differs.
pub fn cover_lookup_keys(key: &str) -> Vec<String> {
    let Some(key) = normalise_cover_key(key) else {
        return Vec::new();
    };
    let mut keys = vec![key.clone()];
    if let Some((artist, album)) = key_halves(&key) {
        if let Some(canonical) = canonical_cover_key(artist, album) {
            if canonical != key {
                keys.push(canonical);
            }
        }
    }
    keys
}

fn key_halves(key: &str) -> Option<(&str, &str)> {
    let mut halves = key.split(COVER_KEY_SEPARATOR);
    let artist = halves.next()?;
    let album = halves.next()?;
    (halves.next().is_none() && !artist.is_empty() && !album.is_empty()).then_some((artist, album))
}

/// Leading URL or watermark tokens, such as `www.example.tk` or
/// `https://example.tk`, that file tags often carry, plus the punctuation that
/// usually surrounds them.
fn strip_leading_noise_token(value: &str) -> Option<&str> {
    let trimmed = value.trim_start();
    let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let token = &trimmed[..token_end];
    if token.is_empty() {
        return None;
    }
    // `! www.example.tk ! Real Artist` must clean to `Real Artist`, so loose
    // punctuation is dropped alongside the watermark it wraps.
    let punctuation_only = !token.chars().any(char::is_alphanumeric);
    if !punctuation_only && !looks_like_domain(token) {
        return None;
    }
    Some(&trimmed[token_end..])
}

fn looks_like_domain(token: &str) -> bool {
    let bare = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && character != '.' && character != '-'
    });
    if bare.contains("://") || bare.to_ascii_lowercase().starts_with("www.") {
        return true;
    }
    let labels = bare.split('.').collect::<Vec<_>>();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
        && labels
            .last()
            .is_some_and(|top| top.len() >= 2 && top.chars().all(|c| c.is_ascii_alphabetic()))
}

fn clean_artist(value: &str) -> String {
    let mut cleaned = value.trim().to_string();
    while let Some(rest) = strip_leading_noise_token(&cleaned) {
        cleaned = rest.to_string();
    }
    cleaned
        .chars()
        .filter(|character| !is_bracket_character(*character))
        .collect::<String>()
        .trim_matches(|character: char| character == '!' || character.is_whitespace())
        .to_string()
}

/// Edition and packaging markers that carry no release identity. A group is
/// dropped only when one of these words appears inside it, so meaningful
/// parentheticals such as `(Live at Pompeii)` survive.
const ALBUM_NOISE_WORDS: &[&str] = &[
    "anniversary",
    "bonus",
    "cd",
    "collector",
    "collectors",
    "deluxe",
    "digital",
    "disc",
    "disk",
    "edition",
    "enhanced",
    "explicit",
    "expanded",
    "hires",
    "hi-res",
    "limited",
    "mono",
    "promo",
    "reissue",
    "release",
    "remaster",
    "remastered",
    "repack",
    "special",
    "stereo",
    "version",
    "vinyl",
    "volume",
];

fn clean_album(value: &str) -> String {
    let mut cleaned = value.trim().to_string();
    loop {
        let trimmed = cleaned.trim_end();
        let Some(closing) = trimmed.chars().last().filter(|c| *c == ')' || *c == ']') else {
            break;
        };
        let opening = if closing == ')' { '(' } else { '[' };
        let Some(start) = trimmed.rfind(opening) else {
            break;
        };
        if !is_noise_group(&trimmed[start + 1..trimmed.len() - 1]) {
            break;
        }
        cleaned = trimmed[..start].to_string();
    }
    loop {
        let trimmed = cleaned.trim_end();
        let Some(segment_start) = trimmed.rfind(" - ") else {
            break;
        };
        if !is_noise_group(&trimmed[segment_start + 3..]) {
            break;
        }
        cleaned = trimmed[..segment_start].to_string();
    }
    cleaned
        .trim_end_matches(|character: char| {
            character.is_whitespace() || character == '-' || character == '_' || character == ','
        })
        .trim()
        .to_string()
}

fn is_noise_group(value: &str) -> bool {
    let lowered = value.trim().to_lowercase();
    if lowered.is_empty() {
        return false;
    }
    lowered
        .split(|character: char| !character.is_alphanumeric() && character != '-')
        .filter(|word| !word.is_empty())
        .any(|word| {
            // Only exact words match, so a meaningful parenthetical such as
            // `(Epic Records)` is never mistaken for a packaging marker.
            ALBUM_NOISE_WORDS.contains(&word)
                || word
                    .strip_suffix('s')
                    .is_some_and(|singular| ALBUM_NOISE_WORDS.contains(&singular))
        })
}

fn is_bracket_character(character: char) -> bool {
    matches!(
        character,
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
    )
}

// ---------------------------------------------------------------------------
// Event validation
// ---------------------------------------------------------------------------

/// Everything needed to assert a cover this host resolved itself.
#[derive(Debug, Clone, Default)]
pub struct CoverClaimFields {
    pub key: String,
    pub art: String,
    pub thumb: String,
    pub mbid: String,
    pub year: String,
    pub genre: String,
    pub collection: String,
    pub source: String,
}

/// Build and sign the kind `30427` event that asserts `fields`.
///
/// This is the publishing half of the same contract [`cover_claim`] reads, so
/// the two sit side by side: a claim this host would refuse to parse is a claim
/// no other client should have to read either.
pub fn cover_event(fields: &CoverClaimFields, keys: &Keys) -> Result<Event, String> {
    let key = normalise_cover_key(&fields.key).ok_or("invalid album cover key")?;
    if !valid_cover_key(&key) {
        return Err("invalid album cover key".into());
    }
    let art = fields.art.trim().to_string();
    if !valid_cover_art_url(&art) {
        return Err("a cover claim needs an HTTPS image URL".into());
    }
    let content = CoverContent {
        protocol: "napstr/1".into(),
        art,
        mbid: claim_text(&fields.mbid),
        year: claim_text(&fields.year),
        genre: claim_text(&fields.genre),
        collection: claim_text(&fields.collection),
        source: claim_text(&fields.source),
        deleted: false,
    };
    let encoded = serde_json::to_string(&content).map_err(|error| error.to_string())?;
    if encoded.len() > COVER_CONTENT_BYTE_LIMIT {
        return Err("this cover claim is too large to publish".into());
    }
    let mut tags = vec![
        Tag::parse(["d", key.as_str()]).map_err(|error| error.to_string())?,
        Tag::parse(["t", COVER_MARKER]).map_err(|error| error.to_string())?,
    ];
    // A thumbnail is optional, and only worth publishing when it too is HTTPS.
    let thumb = fields.thumb.trim();
    if !thumb.is_empty() && valid_cover_art_url(thumb) {
        tags.push(Tag::parse(["thumb", thumb]).map_err(|error| error.to_string())?);
    }
    EventBuilder::new(Kind::from(COVER_KIND), encoded)
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|error| error.to_string())
}

/// Validate a kind `30427` event and turn it into a claim filed under `key`.
///
/// `key` is the verbatim key the caller wants answered. A claim published under
/// the canonical alias is stored and served under the verbatim key exactly as
/// if it had matched it, and a claim for any other album is refused.
pub fn cover_claim(event: &Event, key: &str) -> Option<CoverClaim> {
    let key = normalise_cover_key(key)?;
    if !valid_cover_key(key.as_str()) {
        return None;
    }
    // The event's own coordinate must already be normalized and must be one of
    // the selectors that can answer `key`.
    let identifier = event.tags.identifier()?;
    if normalise_cover_key(identifier).as_deref() != Some(identifier)
        || !cover_lookup_keys(&key)
            .iter()
            .any(|selector| selector == identifier)
    {
        return None;
    }
    let content = serde_json::from_str::<CoverContent>(&event.content).ok()?;
    if !valid_cover_event(event, &content) {
        return None;
    }
    let author = event.pubkey.to_hex();
    let event_id = event.id.to_hex();
    let created_at = event.created_at.as_secs();
    if content.deleted {
        return Some(CoverClaim::Withdrawn {
            key,
            author,
            event_id,
            created_at,
        });
    }
    let cover_file_id = embedded_file_id(event).unwrap_or_default();
    Some(CoverClaim::Art(Box::new(AlbumCover {
        key,
        art: content.art.trim().to_string(),
        thumb: tag_content(event, "thumb").unwrap_or_default(),
        mbid: claim_text(&content.mbid),
        year: claim_text(&content.year),
        genre: claim_text(&content.genre),
        collection: claim_text(&content.collection),
        source: claim_text(&content.source),
        cover_file_id,
        mime: cover_mime(event),
        author,
        event_id,
        created_at,
        seeder: false,
    })))
}

/// The event's own coordinate must be a valid cover key, so a claim can never
/// be filed under a coordinate the author did not sign for.
pub fn valid_cover_key(key: &str) -> bool {
    key_halves(key).is_some() && key.chars().count() <= COVER_KEY_CHARACTER_LIMIT
}

fn valid_cover_event(event: &Event, content: &CoverContent) -> bool {
    if event.kind != Kind::from(COVER_KIND)
        || event.verify().is_err()
        || event.content.len() > COVER_CONTENT_BYTE_LIMIT
        || content.protocol != "napstr/1"
        || !event.tags.hashtags().any(|tag| tag == COVER_MARKER)
        || !event
            .tags
            .identifier()
            .is_some_and(|identifier| normalise_cover_key(identifier).is_some())
    {
        return false;
    }
    // The withdrawal body carries neither an image nor a reference to one, so
    // the art requirement applies only to real claims.
    if content.deleted {
        return true;
    }
    // A cover must resolve to image bytes: either a linked HTTPS URL or an
    // embedded copy published as a catalogue entry.
    match embedded_file_id(event) {
        Some(_) => true,
        None if content.art.is_empty() => false,
        None => valid_cover_art_url(&content.art),
    }
}

/// Only HTTPS URLs are accepted; `http:` and non-URL values are rejected.
fn valid_cover_art_url(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > COVER_ART_URL_CHARACTER_LIMIT
        || value.chars().any(char::is_control)
    {
        return false;
    }
    Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn embedded_file_id(event: &Event) -> Option<String> {
    let value = tag_content(event, "x")?;
    let value = value.trim();
    // The NIP requires a 64-character lowercase hex SHA-256, so an uppercase
    // claim is not accepted even though the bytes would be identical.
    if value.len() != 64
        || !value
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
    {
        return None;
    }
    Some(value.to_string())
}

/// Read the first value of a tag by its raw name, so optional tags such as
/// `thumb` and `m` do not need a dedicated `TagKind` constructor.
fn tag_content(event: &Event, name: &str) -> Option<String> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == TagKind::from(name))
        .and_then(|tag| tag.content())
        .map(|value| value.to_string())
}

/// Untrusted display text is bounded and stripped of control characters rather
/// than rejected, because it is informational only.
fn claim_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(COVER_TEXT_CHARACTER_LIMIT)
        .collect::<String>()
        .trim()
        .to_string()
}

fn cover_mime(event: &Event) -> String {
    tag_content(event, "m")
        .map(|value| claim_text(&value))
        .filter(|value| {
            value.chars().count() <= COVER_MIME_CHARACTER_LIMIT
                && value
                    .to_ascii_lowercase()
                    .starts_with("image/")
        })
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Pick the winning cover for one key.
///
/// A claim from an active seeder of the album always outranks a claim from
/// anybody else; inside a trust class the newest `created_at` wins, with the
/// event id as a deterministic tie-break.
pub fn winning_cover(claims: &[AlbumCover]) -> Option<&AlbumCover> {
    claims.iter().max_by(|left, right| {
        (left.seeder, left.created_at, left.event_id.as_str()).cmp(&(
            right.seeder,
            right.created_at,
            right.event_id.as_str(),
        ))
    })
}

/// Filter a requested key list down to valid, deduplicated keys.
pub fn normalised_request(keys: &[String], limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalised = Vec::new();
    for key in keys {
        let Some(key) = normalise_cover_key(key) else {
            continue;
        };
        if seen.insert(key.clone()) {
            normalised.push(key);
        }
        if normalised.len() >= limit {
            break;
        }
    }
    normalised
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const COVER_COLUMNS: &str = "cover_key,source_pubkey,art,thumb,mbid,year,genre,collection,source,cover_file_id,mime,event_id,created_at,deleted,seeder";

pub(crate) const COVER_MISS_LIFETIME_SECONDS: i64 = 30 * 60;
pub(crate) const COVER_HIT_LIFETIME_SECONDS: i64 = 6 * 60 * 60;

/// The table counting every change to what this computer would answer about
/// album art.
///
/// A companion caches covers, including "this computer has none", so it needs
/// one number that tells it a cached answer may be stale. Asking again on every
/// render would cost a round trip each time, and never asking again is what
/// leaves a phone showing no art for an album this computer found art for
/// minutes earlier.
///
/// Declared next to whichever half of the schema creates the table the trigger
/// watches, because the two halves may run in either order and neither may
/// depend on the other. The second declaration and its seed row are no-ops.
/// Triggers move the counter rather than callers, so no write path can forget
/// to move it.
pub(crate) const COVER_REVISION_TABLE: &str = "\
  CREATE TABLE IF NOT EXISTS cover_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
  );
  INSERT OR IGNORE INTO cover_state(id, revision) VALUES (1, 0);";

/// Add the cover columns, indexes, and the album cover tables to an existing
/// database, then backfill catalogue keys once, when the column is new.
pub(crate) fn initialise_cover_schema(connection: &Connection) -> Result<(), String> {
    let mut backfill = false;
    for column in ["cover_key", "canonical_cover_key"] {
        if !catalogue_column_exists(connection, column)? {
            backfill = true;
        }
        super::ensure_column(
            connection,
            "remote_catalogue",
            column,
            "TEXT NOT NULL DEFAULT ''",
        )?;
    }
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS remote_catalogue_cover_key
               ON remote_catalogue(cover_key);
             CREATE INDEX IF NOT EXISTS remote_catalogue_canonical_cover_key
               ON remote_catalogue(canonical_cover_key);
             -- What this computer resolved from MusicBrainz or the Cover Art
             -- Archive, so a second run never asks the same question twice and
             -- a polite pause is remembered across restarts.
             CREATE TABLE IF NOT EXISTS album_art_lookups (
               cover_key TEXT PRIMARY KEY,
               checked_at TEXT NOT NULL,
               outcome TEXT NOT NULL,
               retry_at TEXT NOT NULL,
               art TEXT NOT NULL DEFAULT '',
               thumb TEXT NOT NULL DEFAULT '',
               mbid TEXT NOT NULL DEFAULT '',
               year TEXT NOT NULL DEFAULT '',
               collection TEXT NOT NULL DEFAULT '',
               source TEXT NOT NULL DEFAULT ''
             );
             -- The albums a window actually put on screen, which is what
             -- \"albums seen in search results\" means. Deliberately not the
             -- catalogue cache: that holds everything ever browsed, which on a
             -- used install is thousands of records this computer does not own.
             CREATE TABLE IF NOT EXISTS cover_watch (
               cover_key TEXT PRIMARY KEY,
               artist TEXT NOT NULL,
               album TEXT NOT NULL,
               noted_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cover_watch_noted_at ON cover_watch(noted_at);",
        )
        .map_err(|error| error.to_string())?;
    // Art this computer resolved for itself is half of what it would report, so
    // a change here is a change a phone caching covers has to hear about. The
    // `album_covers` half is declared where that table is created.
    connection
        .execute_batch(&format!(
            "{COVER_REVISION_TABLE}
             CREATE TRIGGER IF NOT EXISTS album_art_lookups_cover_revision
             AFTER INSERT ON album_art_lookups BEGIN
               UPDATE cover_state SET revision = revision + 1 WHERE id = 1;
             END;
             CREATE TRIGGER IF NOT EXISTS album_art_lookups_cover_revision_updated
             AFTER UPDATE ON album_art_lookups BEGIN
               UPDATE cover_state SET revision = revision + 1 WHERE id = 1;
             END;
             CREATE TRIGGER IF NOT EXISTS album_art_lookups_cover_revision_deleted
             AFTER DELETE ON album_art_lookups BEGIN
               UPDATE cover_state SET revision = revision + 1 WHERE id = 1;
             END;"
        ))
        .map_err(|error| error.to_string())?;
    if backfill {
        backfill_catalogue_cover_keys(connection)?;
    }
    Ok(())
}

fn catalogue_column_exists(connection: &Connection, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare("PRAGMA table_info(remote_catalogue)")
        .map_err(|error| error.to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(columns.iter().any(|existing| existing == column))
}

fn backfill_catalogue_cover_keys(connection: &Connection) -> Result<(), String> {
    const BATCH: usize = 1_000;
    let mut last_rowid = 0i64;
    loop {
        let rows = {
            let mut statement = connection
                .prepare("SELECT rowid,artist,album FROM remote_catalogue WHERE rowid>?1 ORDER BY rowid LIMIT ?2")
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(params![last_rowid, BATCH as i64], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            rows
        };
        if rows.is_empty() {
            return Ok(());
        }
        let batch_len = rows.len();
        for (rowid, artist, album) in rows {
            last_rowid = rowid;
            connection
                .execute(
                    "UPDATE remote_catalogue SET cover_key=?2,canonical_cover_key=?3 WHERE rowid=?1",
                    params![
                        rowid,
                        cover_key(&artist, &album).unwrap_or_default(),
                        canonical_cover_key(&artist, &album).unwrap_or_default()
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        if batch_len < BATCH {
            return Ok(());
        }
    }
}

/// Keys that need a relay query: never checked, or checked longer ago than the
/// hit or miss lifetime allows.
pub(crate) fn stale_cover_keys(
    connection: &Connection,
    keys: &[String],
) -> Result<Vec<String>, String> {
    let now = Utc::now().timestamp();
    let mut stale = Vec::new();
    let mut statement = connection
        .prepare("SELECT checked_at,hit FROM album_cover_queries WHERE cover_key=?1")
        .map_err(|error| error.to_string())?;
    for key in keys {
        let checked: Option<(String, i64)> = statement
            .query_row(params![key], |row| Ok((row.get(0)?, row.get(1)?)))
            .ok();
        let fresh = checked.is_some_and(|(checked_at, hit)| {
            let lifetime = if hit == 1 {
                COVER_HIT_LIFETIME_SECONDS
            } else {
                COVER_MISS_LIFETIME_SECONDS
            };
            let Ok(checked_at) = chrono::DateTime::parse_from_rfc3339(&checked_at) else {
                return false;
            };
            now - checked_at.timestamp() < lifetime
        });
        if !fresh {
            stale.push(key.clone());
        }
    }
    Ok(stale)
}

pub(crate) fn mark_cover_keys_checked(
    connection: &Connection,
    keys: &[String],
    hits: &HashSet<String>,
) -> Result<(), String> {
    let now = Utc::now().to_rfc3339();
    let mut statement = connection
        .prepare(
            "INSERT INTO album_cover_queries(cover_key,checked_at,hit) VALUES(?1,?2,?3)
             ON CONFLICT(cover_key) DO UPDATE SET checked_at=excluded.checked_at,hit=excluded.hit",
        )
        .map_err(|error| error.to_string())?;
    for key in keys {
        statement
            .execute(params![key, now, i64::from(hits.contains(key))])
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// External art resolution cache
// ---------------------------------------------------------------------------
//
// Everything below is the *resolver's* memory, not a claim. Nothing here is
// signed, nothing here is published, and nothing here is ever served as a kind
// `30427`.

/// How long a resolution is trusted before MusicBrainz is asked again. Cover
/// art changes very slowly, so this is measured in weeks.
pub(crate) const ART_FOUND_LIFETIME_SECONDS: i64 = 30 * 24 * 60 * 60;
/// How long "nobody has scanned this record" is trusted. Long, because it is
/// the answer that costs a request to rediscover and never changes quickly.
pub(crate) const ART_NONE_LIFETIME_SECONDS: i64 = 14 * 24 * 60 * 60;

/// A front cover this computer resolved from a third-party API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ArtLookup {
    pub key: String,
    pub art: String,
    pub thumb: String,
    pub mbid: String,
    pub year: String,
    pub collection: String,
    pub source: String,
}

/// What one lookup learned, and therefore when it may be repeated.
pub(crate) enum ArtLookupOutcome<'a> {
    /// The archive had a front cover.
    Found(&'a ArtLookup),
    /// A considered answer: no art exists for this release group.
    NoArt,
    /// A transient failure, parked for `retry_after_seconds` so a throttling
    /// MusicBrainz is not asked the same album again immediately.
    Failed { retry_after_seconds: i64 },
}

/// What the cache already knows about one key.
pub(crate) enum CachedArt {
    /// A resolution worth using without asking anybody.
    Found(Box<ArtLookup>),
    /// Asked recently and answered "none", or parked after a failure.
    Suppressed,
    /// Never asked, or the answer is old enough to ask again.
    Unknown,
}

fn retry_stamp(now: chrono::DateTime<Utc>, seconds: i64) -> String {
    (now + chrono::Duration::seconds(seconds.max(1))).to_rfc3339()
}

/// File what a lookup learned about `key`, and set when it may be tried again.
pub(crate) fn record_art_lookup(
    connection: &Connection,
    key: &str,
    outcome: ArtLookupOutcome<'_>,
) -> Result<(), String> {
    let key = normalise_cover_key(key).ok_or("invalid album cover key")?;
    let now = Utc::now();
    let checked_at = now.to_rfc3339();
    match outcome {
        ArtLookupOutcome::Found(lookup) => {
            connection
                .execute(
                    "INSERT INTO album_art_lookups(cover_key,checked_at,outcome,retry_at,art,thumb,mbid,year,collection,source)
                     VALUES(?1,?2,'found',?3,?4,?5,?6,?7,?8,?9)
                     ON CONFLICT(cover_key) DO UPDATE SET
                       checked_at=excluded.checked_at, outcome='found', retry_at=excluded.retry_at,
                       art=excluded.art, thumb=excluded.thumb, mbid=excluded.mbid,
                       year=excluded.year, collection=excluded.collection, source=excluded.source",
                    params![
                        key,
                        checked_at,
                        retry_stamp(now, ART_FOUND_LIFETIME_SECONDS),
                        lookup.art,
                        lookup.thumb,
                        lookup.mbid,
                        lookup.year,
                        lookup.collection,
                        lookup.source
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        ArtLookupOutcome::NoArt => {
            connection
                .execute(
                    "INSERT INTO album_art_lookups(cover_key,checked_at,outcome,retry_at,art,thumb,mbid,year,collection,source)
                     VALUES(?1,?2,'none',?3,'','','','','','')
                     ON CONFLICT(cover_key) DO UPDATE SET
                       checked_at=excluded.checked_at, outcome='none', retry_at=excluded.retry_at,
                       art='', thumb='', mbid='', year='', collection='', source=''",
                    params![key, checked_at, retry_stamp(now, ART_NONE_LIFETIME_SECONDS)],
                )
                .map_err(|error| error.to_string())?;
        }
        ArtLookupOutcome::Failed {
            retry_after_seconds,
        } => {
            // A failure must never bury a resolution that worked, so a `found`
            // row is left exactly as it is.
            connection
                .execute(
                    "INSERT INTO album_art_lookups(cover_key,checked_at,outcome,retry_at,art,thumb,mbid,year,collection,source)
                     VALUES(?1,?2,'error',?3,'','','','','','')
                     ON CONFLICT(cover_key) DO UPDATE SET
                       checked_at=excluded.checked_at, outcome='error', retry_at=excluded.retry_at
                     WHERE album_art_lookups.outcome <> 'found'",
                    params![key, checked_at, retry_stamp(now, retry_after_seconds)],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// What the cache already knows about `key`, honouring the retry stamp.
pub(crate) fn cached_art(connection: &Connection, key: &str) -> Result<CachedArt, String> {
    let Some(key) = normalise_cover_key(key) else {
        return Ok(CachedArt::Unknown);
    };
    let row = connection
        .query_row(
            "SELECT outcome,retry_at,art,thumb,mbid,year,collection,source
               FROM album_art_lookups WHERE cover_key=?1",
            [&key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((outcome, retry_at, art, thumb, mbid, year, collection, source)) = row else {
        return Ok(CachedArt::Unknown);
    };
    let pending = chrono::DateTime::parse_from_rfc3339(&retry_at)
        .map(|retry_at| retry_at > Utc::now())
        .unwrap_or(false);
    if !pending {
        return Ok(CachedArt::Unknown);
    }
    if outcome == "found" && !art.is_empty() {
        return Ok(CachedArt::Found(Box::new(ArtLookup {
            key,
            art,
            thumb,
            mbid,
            year,
            collection,
            source,
        })));
    }
    Ok(CachedArt::Suppressed)
}

/// A resolution this computer holds for `key`, whatever its age.
///
/// Freshness decides whether MusicBrainz should be asked again; it never
/// withholds a picture this computer already has, because a working image is
/// worth more than a retry stamp. This is what makes "sign what I resolved"
/// possible with lookups switched off.
pub(crate) fn stored_art(connection: &Connection, key: &str) -> Result<Option<ArtLookup>, String> {
    let Some(key) = normalise_cover_key(key) else {
        return Ok(None);
    };
    connection
        .query_row(
            "SELECT art,thumb,mbid,year,collection,source FROM album_art_lookups
               WHERE cover_key=?1 AND art<>''",
            [&key],
            |row| {
                Ok(ArtLookup {
                    key: key.clone(),
                    art: row.get(0)?,
                    thumb: row.get(1)?,
                    mbid: row.get(2)?,
                    year: row.get(3)?,
                    collection: row.get(4)?,
                    source: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

/// Every cover key this computer holds art for, in one query.
///
/// Used to decide what a publish-only pass can actually act on, so its pending
/// list never fills up with albums that cannot progress.
pub(crate) fn stored_art_keys(connection: &Connection) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT cover_key FROM album_art_lookups WHERE art <> ''")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// What a window has shown
// ---------------------------------------------------------------------------

/// How long an album the window showed stays in the worker's queue.
///
/// This is *retention*, not a freshness heuristic. The pending list is built
/// once at the start of a pass, so an album that was on screen while a long pass
/// ran must still be there when the next pass asks — otherwise it is never
/// looked up at all. It is measured in hours for exactly that reason.
pub(crate) const WATCH_RETENTION_HOURS: i64 = 24;

/// Remember the albums a window has just put on screen.
///
/// Returns how many addressable albums were recorded. Keys are computed here,
/// so every caller normalizes them the same way.
pub(crate) fn note_watched_albums(
    connection: &Connection,
    albums: &[(String, String)],
) -> Result<usize, String> {
    let now = Utc::now().to_rfc3339();
    let mut statement = connection
        .prepare(
            "INSERT INTO cover_watch(cover_key,artist,album,noted_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(cover_key) DO UPDATE SET
               artist=excluded.artist, album=excluded.album, noted_at=excluded.noted_at",
        )
        .map_err(|error| error.to_string())?;
    let mut noted = 0;
    for (artist, album) in albums {
        let Some(key) = cover_key(artist, album) else {
            continue;
        };
        statement
            .execute(params![key, artist.trim(), album.trim(), now])
            .map_err(|error| error.to_string())?;
        noted += 1;
    }
    Ok(noted)
}

/// Albums a window showed recently, newest first, as `(key, artist, album)`.
pub(crate) fn watched_albums(
    connection: &Connection,
) -> Result<Vec<(String, String, String)>, String> {
    let cutoff = Utc::now() - chrono::Duration::hours(WATCH_RETENTION_HOURS);
    let mut statement = connection
        .prepare("SELECT cover_key,artist,album,noted_at FROM cover_watch ORDER BY noted_at DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut albums = Vec::new();
    for row in rows {
        let (key, artist, album, noted_at) = row.map_err(|error| error.to_string())?;
        // Compared as timestamps rather than as text, so a differing fraction of
        // a second cannot decide whether an album is still queued.
        let recent = chrono::DateTime::parse_from_rfc3339(&noted_at)
            .map(|noted_at| noted_at > cutoff)
            .unwrap_or(false);
        if recent {
            albums.push((key, artist, album));
        }
    }
    Ok(albums)
}

/// Drop watch entries that have aged out, so the table cannot grow forever.
pub(crate) fn prune_watch(connection: &Connection) -> Result<usize, String> {
    // A coarse text comparison is fine at a day's resolution: a sub-second
    // boundary error would only leave one entry in the table a moment longer.
    let cutoff = (Utc::now() - chrono::Duration::hours(WATCH_RETENTION_HOURS)).to_rfc3339();
    connection
        .execute("DELETE FROM cover_watch WHERE noted_at < ?1", params![cutoff])
        .map_err(|error| error.to_string())
}

/// Keys recently answered "no art", or parked after a failure. Offering those
/// as candidates again would only spend requests to learn the same thing.
///
/// Read in one pass rather than one query per key: the table only ever holds
/// albums this computer has asked about, so scanning it is cheap even when the
/// caller offers a whole library.
pub(crate) fn suppressed_art_keys(
    connection: &Connection,
    keys: &[String],
) -> Result<HashSet<String>, String> {
    if keys.is_empty() {
        return Ok(HashSet::new());
    }
    let wanted = keys.iter().map(String::as_str).collect::<HashSet<_>>();
    let now = Utc::now();
    let mut statement = connection
        .prepare("SELECT cover_key,outcome,retry_at FROM album_art_lookups")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut suppressed = HashSet::new();
    for row in rows {
        let (key, outcome, retry_at) = row.map_err(|error| error.to_string())?;
        if outcome == "found" || !wanted.contains(key.as_str()) {
            continue;
        }
        if chrono::DateTime::parse_from_rfc3339(&retry_at)
            .map(|retry_at| retry_at > now)
            .unwrap_or(false)
        {
            suppressed.insert(key);
        }
    }
    Ok(suppressed)
}

/// A resolution this computer holds, shaped for display and for the phone.
///
/// `author` and `event_id` are empty on purpose: this is not a claim by anybody,
/// and a client that needs an author must ask a relay instead.
pub(crate) fn cover_from_art_lookup(lookup: &ArtLookup) -> AlbumCover {
    AlbumCover {
        key: lookup.key.clone(),
        art: lookup.art.clone(),
        thumb: lookup.thumb.clone(),
        mbid: lookup.mbid.clone(),
        year: lookup.year.clone(),
        genre: String::new(),
        collection: lookup.collection.clone(),
        source: lookup.source.clone(),
        cover_file_id: String::new(),
        mime: String::new(),
        author: String::new(),
        event_id: String::new(),
        created_at: 0,
        seeder: false,
    }
}

/// How many times what this computer would answer about album art has changed.
///
/// A companion compares this against the value it last saw: any difference
/// means its cached covers — including the albums it was told nobody had art
/// for — are worth asking about again. Deliberately coarse, so it says "ask
/// again" rather than which album changed, and stored rather than derived, so
/// it survives a restart.
pub(crate) fn cover_revision(connection: &Connection) -> Result<u64, String> {
    let present = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cover_state'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?;
    if present == 0 {
        // A database whose cover schema has not been created yet is a host with
        // nothing to report. It must not be an error, because this is read while
        // answering a phone's status, and a status that fails reads as an offline
        // desktop. Zero already means "never invalidated" on the wire.
        return Ok(0);
    }
    let revision = connection
        .query_row("SELECT revision FROM cover_state WHERE id=1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(|error| error.to_string())?;
    Ok(revision.unwrap_or(0).max(0) as u64)
}

/// Resolutions held for `keys`, whatever their age.
///
/// Freshness is the scanner's business, not the renderer's: an old answer is
/// still the best picture of that album this computer has.
pub(crate) fn resolved_covers(
    connection: &Connection,
    keys: &[String],
) -> Result<Vec<AlbumCover>, String> {
    let mut covers = Vec::new();
    for key in keys {
        if let Some(lookup) = stored_art(connection, key)? {
            covers.push(cover_from_art_lookup(&lookup));
        }
    }
    Ok(covers)
}

/// Store the newest claim from every author for the given key/event pairs.
///
/// Withdrawals are stored as tombstones so this author's older claim is never
/// offered again, and a later re-claim replaces the tombstone by `created_at`.
pub(crate) fn store_cover_events(
    connection: &Connection,
    pairs: &[(String, Event)],
) -> Result<(), String> {
    let now = Utc::now().to_rfc3339();
    for (key, event) in pairs {
        let Some(claim) = cover_claim(event, key) else {
            continue;
        };
        match claim {
            CoverClaim::Art(cover) => {
                connection
                    .execute(
                        "INSERT INTO album_covers(cover_key,source_pubkey,art,thumb,mbid,year,genre,collection,source,cover_file_id,mime,event_id,created_at,deleted,seeder,seen_at)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0,0,?14)
                         ON CONFLICT(cover_key,source_pubkey) DO UPDATE SET
                           art=excluded.art,thumb=excluded.thumb,mbid=excluded.mbid,year=excluded.year,
                           genre=excluded.genre,collection=excluded.collection,source=excluded.source,
                           cover_file_id=excluded.cover_file_id,mime=excluded.mime,event_id=excluded.event_id,
                           created_at=excluded.created_at,deleted=0,seen_at=excluded.seen_at
                         WHERE excluded.created_at >= album_covers.created_at",
                        params![
                            cover.key,
                            cover.author,
                            cover.art,
                            cover.thumb,
                            cover.mbid,
                            cover.year,
                            cover.genre,
                            cover.collection,
                            cover.source,
                            cover.cover_file_id,
                            cover.mime,
                            cover.event_id,
                            cover.created_at as i64,
                            now
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
            CoverClaim::Withdrawn {
                key,
                author,
                event_id,
                created_at,
            } => {
                connection
                    .execute(
                        "INSERT INTO album_covers(cover_key,source_pubkey,art,thumb,mbid,year,genre,collection,source,cover_file_id,mime,event_id,created_at,deleted,seeder,seen_at)
                         VALUES(?1,?2,'','','','','','','','','',?3,?4,1,0,?5)
                         ON CONFLICT(cover_key,source_pubkey) DO UPDATE SET
                           art='',thumb='',mbid='',year='',genre='',collection='',source='',
                           cover_file_id='',mime='',event_id=excluded.event_id,
                           created_at=excluded.created_at,deleted=1,seen_at=excluded.seen_at
                         WHERE excluded.created_at >= album_covers.created_at",
                        params![key, author, event_id, created_at as i64, now],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

/// Winning, non-withdrawn cover for every requested key, honouring the local
/// block list.
pub(crate) fn load_cover_claims(
    connection: &Connection,
    keys: &[String],
) -> Result<Vec<AlbumCover>, String> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let blocked = blocked_pubkeys(connection)?;
    let mut statement = connection
        .prepare(&format!(
            "SELECT {} FROM album_covers WHERE cover_key=?1 AND deleted=0",
            COVER_COLUMNS
        ))
        .map_err(|error| error.to_string())?;
    let mut covers = Vec::new();
    for key in keys {
        let claims = statement
            .query_map(params![key], |row| {
                Ok(AlbumCover {
                    key: row.get(0)?,
                    author: row.get(1)?,
                    art: row.get(2)?,
                    thumb: row.get(3)?,
                    mbid: row.get(4)?,
                    year: row.get(5)?,
                    genre: row.get(6)?,
                    collection: row.get(7)?,
                    source: row.get(8)?,
                    cover_file_id: row.get(9)?,
                    mime: row.get(10)?,
                    event_id: row.get(11)?,
                    created_at: row.get::<_, i64>(12)? as u64,
                    seeder: row.get::<_, i64>(14)? == 1,
                })
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let claims = claims
            .into_iter()
            .filter(|claim| !blocked.contains(&claim.author))
            .collect::<Vec<_>>();
        if let Some(winner) = winning_cover(&claims) {
            covers.push(winner.clone());
        }
    }
    covers.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(covers)
}

/// Catalogue rows whose verbatim or canonical key matches, as
/// `(file_id, source_pubkey)` pairs. Availability heartbeats then decide which
/// of these publishers is an active seeder of the album.
pub(crate) fn album_seeder_candidates(
    connection: &Connection,
    keys: &[String],
) -> Result<HashMap<String, Vec<(String, String)>>, String> {
    let mut candidates: HashMap<String, Vec<(String, String)>> = HashMap::new();
    if keys.is_empty() {
        return Ok(candidates);
    }
    let mut statement = connection
        .prepare("SELECT file_id,source_pubkey FROM remote_catalogue WHERE cover_key=?1 OR canonical_cover_key=?1 LIMIT 500")
        .map_err(|error| error.to_string())?;
    for key in keys {
        let rows = statement
            .query_map(params![key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        if !rows.is_empty() {
            candidates.insert(key.clone(), rows);
        }
    }
    Ok(candidates)
}

/// Remember a claim as seeder-authored so an offline read still prefers it.
pub(crate) fn mark_cover_seeder(
    connection: &Connection,
    key: &str,
    author: &str,
) -> Result<(), String> {
    connection
        .execute(
            "UPDATE album_covers SET seeder=1 WHERE cover_key=?1 AND source_pubkey=?2",
            params![key, author],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn blocked_pubkeys(connection: &Connection) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT pubkey FROM blocked_pubkeys")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_event(keys: &Keys, key: &str, content: serde_json::Value) -> Event {
        EventBuilder::new(Kind::from(COVER_KIND), content.to_string())
            .tags(vec![
                Tag::identifier(key),
                Tag::hashtag(COVER_MARKER),
                Tag::parse(["alt", "Napstr album cover assertion"]).unwrap(),
            ])
            .sign_with_keys(keys)
            .unwrap()
    }

    fn cover_content() -> serde_json::Value {
        serde_json::json!({
            "protocol": "napstr/1",
            "art": "https://is1-ssl.mzstatic.com/image/thumb/Music/cover.jpg",
            "mbid": "f4a7b0d2-0000-0000-0000-000000000000",
            "year": "2007",
            "genre": "Rock",
            "collection": "City of Echoes",
            "source": "itunes",
        })
    }

    #[test]
    fn cover_keys_fold_case_and_reject_ambiguous_halves() {
        assert_eq!(
            cover_key("  Pink Floyd ", "Animals").as_deref(),
            Some("pink floyd|animals")
        );
        assert_eq!(cover_key("BEYONCÉ", "Lemonade").as_deref(), Some("beyoncé|lemonade"));
        // Edition markers and whitespace runs are preserved verbatim.
        assert_eq!(
            cover_key("Artist", "Album  (Deluxe Edition)").as_deref(),
            Some("artist|album  (deluxe edition)")
        );
        assert_eq!(cover_key("", "Album"), None);
        assert_eq!(cover_key("Artist", "   "), None);
        assert_eq!(cover_key("A|B", "Album"), None);
        assert_eq!(
            cover_key(&"a".repeat(299), "album"),
            None,
            "a key longer than 300 characters is not addressable"
        );
    }

    #[test]
    fn canonical_alias_cleans_watermarks_and_edition_markers() {
        assert_eq!(
            canonical_cover_key("! www.example.tk ! Real Artist", "Album (Deluxe Edition)")
                .as_deref(),
            Some("real artist|album")
        );
        assert_eq!(
            canonical_cover_key("Real Artist", "Album").as_deref(),
            Some("real artist|album")
        );
        // A leading bracket marker is packaging too, but the separator keeps the
        // album half intact here; only the trailing `- Disc 2` is dropped.
        assert_eq!(
            canonical_cover_key("Artist", "[2023 Remaster] Album - Disc 2").as_deref(),
            Some("artist|[2023 remaster] album")
        );
        assert_eq!(
            canonical_cover_key("Artist", "Live Album - Disc 2").as_deref(),
            Some("artist|live album")
        );
        // A meaningful parenthetical is kept.
        assert_eq!(
            canonical_cover_key("Artist", "Album (Live at Pompeii)").as_deref(),
            Some("artist|album (live at pompeii)")
        );
    }

    #[test]
    fn lookup_keys_include_both_the_verbatim_key_and_the_alias() {
        let keys = cover_lookup_keys("artist|album (deluxe edition)");
        assert_eq!(
            keys,
            vec!["artist|album (deluxe edition)".to_string(), "artist|album".to_string()]
        );
        assert_eq!(cover_lookup_keys("not-a-key"), Vec::<String>::new());
        assert_eq!(
            normalised_request(
                &[" Artist | Album ".to_string(), "artist|album".to_string(), "junk".to_string()],
                10
            ),
            vec!["artist|album".to_string()]
        );
    }

    #[test]
    fn cover_claims_validate_the_marker_kind_and_shape() {
        let keys = Keys::generate();
        let key = "artist|album";
        let event = signed_event(&keys, key, cover_content());
        assert!(matches!(
            cover_claim(&event, key),
            Some(CoverClaim::Art(_))
        ));

        let wrong_kind = EventBuilder::new(Kind::from(30421), "x")
            .tags(vec![Tag::identifier(key), Tag::hashtag(COVER_MARKER)])
            .sign_with_keys(&keys)
            .unwrap();
        assert!(cover_claim(&wrong_kind, key).is_none());

        let missing_marker = EventBuilder::new(Kind::from(COVER_KIND), cover_content().to_string())
            .tag(Tag::identifier(key))
            .sign_with_keys(&keys)
            .unwrap();
        assert!(cover_claim(&missing_marker, key).is_none());

        let mut wrong_protocol = cover_content();
        wrong_protocol["protocol"] = serde_json::json!("napstr/2");
        assert!(cover_claim(&signed_event(&keys, key, wrong_protocol), key).is_none());

        // The coordinate the author signed for must itself be a valid key.
        for broken in ["artist", "artist|", "|album", "a|b|c"] {
            let broken_event = signed_event(&keys, broken, cover_content());
            assert!(
                cover_claim(&broken_event, broken).is_none(),
                "{broken} is not an addressable cover key"
            );
        }

        let mut padded = cover_content();
        padded["source"] = serde_json::json!("x".repeat(COVER_CONTENT_BYTE_LIMIT + 1));
        assert!(cover_claim(&signed_event(&keys, key, padded), key).is_none());
    }

    #[test]
    fn covers_require_https_art_or_an_embedded_copy() {
        let keys = Keys::generate();
        let key = "artist|album";

        let mut insecure = cover_content();
        insecure["art"] = serde_json::json!("http://example.com/cover.jpg");
        assert!(cover_claim(&signed_event(&keys, key, insecure), key).is_none());

        let mut relative = cover_content();
        relative["art"] = serde_json::json!("cover.jpg");
        assert!(cover_claim(&signed_event(&keys, key, relative), key).is_none());

        let mut missing = cover_content();
        missing["art"] = serde_json::json!("");
        assert!(cover_claim(&signed_event(&keys, key, missing), key).is_none());

        // ...but an embedded image published as a catalogue entry is enough.
        let hash = "ab".repeat(32);
        let embedded = EventBuilder::new(
            Kind::from(COVER_KIND),
            serde_json::json!({"protocol": "napstr/1", "source": "embedded"}).to_string(),
        )
        .tags(vec![
            Tag::identifier(key),
            Tag::hashtag(COVER_MARKER),
            Tag::parse(["x", hash.as_str()]).unwrap(),
            Tag::parse(["m", "image/jpeg"]).unwrap(),
        ])
        .sign_with_keys(&keys)
        .unwrap();
        let Some(CoverClaim::Art(cover)) = cover_claim(&embedded, key) else {
            panic!("an embedded cover is a valid claim");
        };
        assert_eq!(cover.cover_file_id, hash);
        assert_eq!(cover.art, "");
        assert_eq!(cover_mime(&embedded), "image/jpeg");

        // The MIME hint is informational, so a non-image value is dropped
        // rather than trusted, and a missing `m` leaves it empty.
        let mistyped = EventBuilder::new(
            Kind::from(COVER_KIND),
            serde_json::json!({"protocol": "napstr/1"}).to_string(),
        )
        .tags(vec![
            Tag::identifier(key),
            Tag::hashtag(COVER_MARKER),
            Tag::parse(["x", hash.as_str()]).unwrap(),
            Tag::parse(["m", "text/html"]).unwrap(),
        ])
        .sign_with_keys(&keys)
        .unwrap();
        assert_eq!(cover_mime(&mistyped), "");
        let untyped = EventBuilder::new(
            Kind::from(COVER_KIND),
            serde_json::json!({"protocol": "napstr/1"}).to_string(),
        )
        .tags(vec![
            Tag::identifier(key),
            Tag::hashtag(COVER_MARKER),
            Tag::parse(["x", hash.as_str()]).unwrap(),
        ])
        .sign_with_keys(&keys)
        .unwrap();
        assert_eq!(cover_mime(&untyped), "");

        // A malformed or uppercase hash is not a usable embedded reference.
        for bad_hash in ["ab".repeat(31), "AB".repeat(32), "zz".repeat(32)] {
            let broken = EventBuilder::new(
                Kind::from(COVER_KIND),
                serde_json::json!({"protocol": "napstr/1"}).to_string(),
            )
            .tags(vec![
                Tag::identifier(key),
                Tag::hashtag(COVER_MARKER),
                Tag::parse(["x", bad_hash.as_str()]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .unwrap();
            assert!(cover_claim(&broken, key).is_none());
        }
    }

    #[test]
    fn untrusted_display_text_is_bounded_and_carries_no_controls() {
        let keys = Keys::generate();
        let key = "artist|album";
        let mut content = cover_content();
        content["genre"] = serde_json::json!("Rock\u{202e}\u{0000}");
        content["collection"] = serde_json::json!("x".repeat(500));
        let Some(CoverClaim::Art(cover)) = cover_claim(&signed_event(&keys, key, content), key)
        else {
            panic!("display text must not invalidate a claim");
        };
        assert_eq!(cover.genre, "Rock\u{202e}");
        assert_eq!(cover.collection.chars().count(), COVER_TEXT_CHARACTER_LIMIT);
    }

    #[test]
    fn a_canonical_alias_claim_satisfies_the_verbatim_key() {
        let keys = Keys::generate();
        let canonical = "artist|album";
        let verbatim = "artist|album (deluxe edition)";
        let event = signed_event(&keys, canonical, cover_content());
        assert!(cover_claim(&event, canonical).is_some());
        let Some(CoverClaim::Art(cover)) = cover_claim(&event, verbatim) else {
            panic!("a canonical alias match must satisfy the verbatim key");
        };
        assert_eq!(cover.key, verbatim);

        // A claim for another album is never filed under this key.
        let unrelated = signed_event(&keys, "artist|other", cover_content());
        assert!(cover_claim(&unrelated, verbatim).is_none());
    }

    #[test]
    fn withdrawals_withdraw_the_authors_whole_coordinate() {
        let keys = Keys::generate();
        let key = "artist|album";
        let withdrawal = EventBuilder::new(
            Kind::from(COVER_KIND),
            serde_json::json!({"protocol": "napstr/1", "deleted": true}).to_string(),
        )
        .tags(vec![Tag::identifier(key), Tag::hashtag(COVER_MARKER)])
        .sign_with_keys(&keys)
        .unwrap();
        let Some(CoverClaim::Withdrawn {
            key: withdrawn_key,
            author,
            ..
        }) = cover_claim(&withdrawal, key)
        else {
            panic!("a withdrawal body must be recognised");
        };
        assert_eq!(withdrawn_key, key);
        assert_eq!(author, keys.public_key().to_hex());
    }

    #[test]
    fn seeder_authored_covers_win_inside_a_key() {
        let older_seeder = AlbumCover {
            seeder: true,
            created_at: 10,
            event_id: "aa".into(),
            ..seedless_cover("artist|album", 30, "bb")
        };
        let newer_other = AlbumCover {
            seeder: false,
            created_at: 20,
            event_id: "cc".into(),
            ..seedless_cover("artist|album", 20, "cc")
        };
        let newest_other = AlbumCover {
            seeder: false,
            created_at: 30,
            event_id: "dd".into(),
            ..seedless_cover("artist|album", 30, "dd")
        };
        let claims = vec![newest_other.clone(), older_seeder.clone(), newer_other];
        assert_eq!(
            winning_cover(&claims).map(|cover| cover.event_id.as_str()),
            Some("aa")
        );

        // Inside one trust class the newest claim wins, and ties break on the
        // event id so repeated reads agree.
        let claims = vec![
            seedless_cover("artist|album", 20, "cc"),
            seedless_cover("artist|album", 20, "bb"),
        ];
        assert_eq!(
            winning_cover(&claims).map(|cover| cover.event_id.as_str()),
            Some("cc")
        );
        assert_eq!(winning_cover(&[]), None);
    }

    fn seedless_cover(key: &str, created_at: u64, event_id: &str) -> AlbumCover {
        AlbumCover {
            key: key.to_string(),
            art: format!("https://example.com/{event_id}.jpg"),
            thumb: String::new(),
            mbid: String::new(),
            year: String::new(),
            genre: String::new(),
            collection: String::new(),
            source: "itunes".into(),
            cover_file_id: String::new(),
            mime: String::new(),
            author: "ab".repeat(32),
            event_id: event_id.to_string(),
            created_at,
            seeder: false,
        }
    }

    /// Mirrors `initialise_database`: the main schema owns the block lists, the
    /// network schema owns the catalogue and the cover tables.
    fn cover_database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE blocked_pubkeys (pubkey TEXT PRIMARY KEY, reason TEXT NOT NULL, created_at TEXT NOT NULL);",
            )
            .unwrap();
        crate::network::initialise_network_schema(&connection).unwrap();
        connection
    }

    fn insert_cover_row(
        connection: &Connection,
        key: &str,
        author: &str,
        created_at: u64,
        art: &str,
        deleted: bool,
        seeder: bool,
    ) {
        let deleted = if deleted { 1 } else { 0 };
        let seeder = if seeder { 1 } else { 0 };
        connection
            .execute(
                "INSERT INTO album_covers(cover_key,source_pubkey,art,thumb,mbid,year,genre,collection,source,cover_file_id,mime,event_id,created_at,deleted,seeder,seen_at)
                 VALUES(?1,?2,?3,'','','','','','itunes','','',?4,?5,?6,?7,?8)",
                params![
                    key,
                    author,
                    art,
                    format!("{:064x}", created_at),
                    created_at as i64,
                    deleted,
                    seeder,
                    Utc::now().to_rfc3339()
                ],
            )
            .unwrap();
    }

    #[test]
    fn stored_covers_follow_the_nip_trust_order() {
        let connection = cover_database();
        let seeder_author = "aa".repeat(32);
        let other = "bb".repeat(32);
        let key = "artist|album".to_string();
        insert_cover_row(
            &connection,
            &key,
            &other,
            200,
            "https://example.com/newest.jpg",
            false,
            false,
        );
        insert_cover_row(
            &connection,
            &key,
            &seeder_author,
            100,
            "https://example.com/seeder.jpg",
            false,
            true,
        );

        let covers = load_cover_claims(&connection, &[key.clone()]).unwrap();
        assert_eq!(covers.len(), 1);
        assert_eq!(
            covers[0].art, "https://example.com/seeder.jpg",
            "a seeder-authored claim outranks a newer claim from somebody else"
        );
        assert!(covers[0].seeder);

        // Inside one trust class the newest claim wins instead.
        connection
            .execute("UPDATE album_covers SET seeder=0", [])
            .unwrap();
        let covers = load_cover_claims(&connection, &[key]).unwrap();
        assert_eq!(covers[0].art, "https://example.com/newest.jpg");
    }

    #[test]
    fn withdrawals_and_blocks_hide_stored_covers() {
        let connection = cover_database();
        let withdrawn = "aa".repeat(32);
        let other = "bb".repeat(32);
        let key = "artist|album".to_string();
        insert_cover_row(
            &connection,
            &key,
            &withdrawn,
            200,
            "https://example.com/withdrawn.jpg",
            true,
            false,
        );
        insert_cover_row(
            &connection,
            &key,
            &other,
            100,
            "https://example.com/kept.jpg",
            false,
            false,
        );

        let covers = load_cover_claims(&connection, &[key.clone()]).unwrap();
        assert_eq!(
            covers[0].art, "https://example.com/kept.jpg",
            "a withdrawn coordinate is never offered, so the next author wins"
        );

        // Blocking an author withdraws their claim from this device too.
        connection
            .execute(
                "INSERT INTO blocked_pubkeys(pubkey,reason,created_at) VALUES(?1,'test',?2)",
                params![other, Utc::now().to_rfc3339()],
            )
            .unwrap();
        assert!(load_cover_claims(&connection, &[key]).unwrap().is_empty());
    }

    #[test]
    fn cover_events_replace_only_their_own_newer_coordinates() {
        let connection = cover_database();
        let keys = Keys::generate();
        let author = keys.public_key().to_hex();
        let key = "artist|album".to_string();
        let event = signed_event(&keys, &key, cover_content());
        let withdrawal = EventBuilder::new(
            Kind::from(COVER_KIND),
            serde_json::json!({"protocol": "napstr/1", "deleted": true}).to_string(),
        )
        .tags(vec![Tag::identifier(&key), Tag::hashtag(COVER_MARKER)])
        .sign_with_keys(&keys)
        .unwrap();

        // A claim that is newer than the event a relay just handed us survives,
        // so a stale replay can never roll a coordinate backwards.
        let far_future = 4_102_444_800; // year 2100, still a positive i64
        insert_cover_row(
            &connection,
            &key,
            &author,
            far_future,
            "https://example.com/stored.jpg",
            false,
            false,
        );
        store_cover_events(&connection, &[(key.clone(), event.clone())]).unwrap();
        let covers = load_cover_claims(&connection, &[key.clone()]).unwrap();
        assert_eq!(covers[0].art, "https://example.com/stored.jpg");

        // The same holds for an older withdrawal body.
        store_cover_events(&connection, &[(key.clone(), withdrawal.clone())]).unwrap();
        assert_eq!(
            load_cover_claims(&connection, &[key.clone()]).unwrap().len(),
            1,
            "an older withdrawal cannot retract a newer claim"
        );

        // Once the withdrawal is the newest event at the coordinate it wins,
        // and a later claim replaces the tombstone.
        connection
            .execute(
                "UPDATE album_covers SET created_at=1 WHERE cover_key=?1",
                params![key],
            )
            .unwrap();
        store_cover_events(&connection, &[(key.clone(), withdrawal)]).unwrap();
        assert!(load_cover_claims(&connection, &[key.clone()])
            .unwrap()
            .is_empty());

        store_cover_events(&connection, &[(key.clone(), event)]).unwrap();
        let covers = load_cover_claims(&connection, &[key]).unwrap();
        assert_eq!(covers[0].art, cover_content()["art"].as_str().unwrap());
    }

    #[test]
    fn cover_queries_remember_misses_and_hits() {
        let connection = cover_database();
        let key = "artist|album".to_string();
        assert_eq!(
            stale_cover_keys(&connection, std::slice::from_ref(&key)).unwrap(),
            vec![key.clone()],
            "a key that was never queried is always stale"
        );

        let hits = HashSet::from([key.clone()]);
        mark_cover_keys_checked(&connection, std::slice::from_ref(&key), &hits).unwrap();
        assert!(stale_cover_keys(&connection, std::slice::from_ref(&key))
            .unwrap()
            .is_empty());

        // A hit stays fresh for hours; a miss expires in half an hour.
        let an_hour_ago = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        connection
            .execute(
                "UPDATE album_cover_queries SET checked_at=?1 WHERE cover_key=?2",
                params![an_hour_ago, key],
            )
            .unwrap();
        assert!(stale_cover_keys(&connection, std::slice::from_ref(&key))
            .unwrap()
            .is_empty());
        connection
            .execute(
                "UPDATE album_cover_queries SET hit=0 WHERE cover_key=?1",
                params![key],
            )
            .unwrap();
        assert_eq!(
            stale_cover_keys(&connection, std::slice::from_ref(&key)).unwrap(),
            vec![key]
        );
    }

    #[test]
    fn catalogue_cover_keys_are_backfilled_for_existing_rows() {
        let connection = Connection::open_in_memory().unwrap();
        // The shape of a database written before cover keys existed.
        connection
            .execute_batch(
                "CREATE TABLE remote_catalogue (
                   file_id TEXT NOT NULL, source_pubkey TEXT NOT NULL, filename TEXT NOT NULL,
                   title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT NOT NULL, format TEXT NOT NULL,
                   mime TEXT NOT NULL, size INTEGER NOT NULL, license TEXT NOT NULL, event_id TEXT NOT NULL,
                   seen_at TEXT NOT NULL, PRIMARY KEY(file_id, source_pubkey)
                 );
                 INSERT INTO remote_catalogue VALUES
                   ('aa','bb','song.mp3','Song','  Pink Floyd ','Animals (Deluxe Edition)','mp3','audio/mpeg',10,'unspecified','cc','now');",
            )
            .unwrap();

        initialise_cover_schema(&connection).unwrap();

        let (key, canonical): (String, String) = connection
            .query_row(
                "SELECT cover_key,canonical_cover_key FROM remote_catalogue WHERE file_id='aa'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(key, "pink floyd|animals (deluxe edition)");
        assert_eq!(canonical, "pink floyd|animals");

        // Re-running the migration is a no-op that must not rewrite the column.
        connection
            .execute(
                "UPDATE remote_catalogue SET cover_key='sentinel' WHERE file_id='aa'",
                [],
            )
            .unwrap();
        initialise_cover_schema(&connection).unwrap();
        let key: String = connection
            .query_row(
                "SELECT cover_key FROM remote_catalogue WHERE file_id='aa'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(key, "sentinel");
    }

    #[test]
    fn the_cover_revision_moves_whenever_the_covers_are_written() {
        // Coarse on purpose. A needless re-ask costs a phone one batched call;
        // a change that went unannounced is a phone showing no art with nothing
        // to tell it why, which is the failure this exists to prevent.
        let connection = cover_database();
        let key = "artist|album";
        let keys = Keys::generate();
        let event = signed_event(&keys, key, cover_content());

        let settled = cover_revision(&connection).unwrap();
        store_cover_events(&connection, &[(key.to_string(), event.clone())]).unwrap();
        let claimed = cover_revision(&connection).unwrap();
        assert!(claimed > settled, "a stored claim must move the revision");

        // Writing the same claim again changes nothing, and still moves it.
        store_cover_events(&connection, &[(key.to_string(), event)]).unwrap();
        assert!(cover_revision(&connection).unwrap() > claimed);

        let claimed = cover_revision(&connection).unwrap();
        record_art_lookup(
            &connection,
            key,
            ArtLookupOutcome::Found(&art_lookup(
                key,
                "https://coverartarchive.org/release/x/1.jpg",
            )),
        )
        .unwrap();
        assert!(
            cover_revision(&connection).unwrap() > claimed,
            "art resolved without being signed must move it too"
        );
    }

    #[test]
    fn an_uninitialised_database_reports_no_cover_revision() {
        // Read while answering a phone's status, so a missing table has to read
        // as "nothing to report" rather than as an error: a status that fails
        // reads as an offline desktop, and zero already means "not invalidated".
        let connection = Connection::open_in_memory().unwrap();
        assert_eq!(cover_revision(&connection).unwrap(), 0);
    }

    fn art_lookup(key: &str, art: &str) -> ArtLookup {
        ArtLookup {
            key: key.to_string(),
            art: art.to_string(),
            thumb: "https://archive.org/thumb.jpg".to_string(),
            mbid: "f4a7b0d2-0000-0000-0000-000000000000".to_string(),
            year: "2007".to_string(),
            collection: "City of Echoes".to_string(),
            source: "musicbrainz".to_string(),
        }
    }

    #[test]
    fn art_lookups_remember_answers_so_musicbrainz_is_asked_once() {
        let connection = cover_database();
        let key = "artist|album".to_string();
        assert!(matches!(
            cached_art(&connection, &key).unwrap(),
            CachedArt::Unknown
        ));

        record_art_lookup(
            &connection,
            &key,
            ArtLookupOutcome::Found(&art_lookup(&key, "https://archive.org/front.jpg")),
        )
        .unwrap();
        let CachedArt::Found(found) = cached_art(&connection, &key).unwrap() else {
            panic!("a recorded resolution must be offered back");
        };
        assert_eq!(found.art, "https://archive.org/front.jpg");
        assert!(suppressed_art_keys(&connection, std::slice::from_ref(&key))
            .unwrap()
            .is_empty());

        // A later failure must not bury the resolution that worked.
        record_art_lookup(
            &connection,
            &key,
            ArtLookupOutcome::Failed {
                retry_after_seconds: 60,
            },
        )
        .unwrap();
        assert!(matches!(
            cached_art(&connection, &key).unwrap(),
            CachedArt::Found(_)
        ));
    }

    #[test]
    fn art_lookups_park_albums_that_have_no_art_or_failed() {
        let connection = cover_database();
        let empty = "artist|nowhere".to_string();
        let broken = "artist|offline".to_string();
        record_art_lookup(&connection, &empty, ArtLookupOutcome::NoArt).unwrap();
        record_art_lookup(
            &connection,
            &broken,
            ArtLookupOutcome::Failed {
                retry_after_seconds: 900,
            },
        )
        .unwrap();
        assert!(matches!(
            cached_art(&connection, &empty).unwrap(),
            CachedArt::Suppressed
        ));
        assert!(matches!(
            cached_art(&connection, &broken).unwrap(),
            CachedArt::Suppressed
        ));
        assert_eq!(
            suppressed_art_keys(&connection, &[empty.clone(), broken.clone()])
                .unwrap()
                .len(),
            2,
            "neither album should be offered again while its answer is fresh"
        );

        // Once the retry stamp passes, the album is a candidate again.
        let past = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        connection
            .execute("UPDATE album_art_lookups SET retry_at=?1", params![past])
            .unwrap();
        assert!(matches!(
            cached_art(&connection, &empty).unwrap(),
            CachedArt::Unknown
        ));
        assert!(suppressed_art_keys(&connection, &[empty]).unwrap().is_empty());
    }

    #[test]
    fn resolved_covers_outlive_their_freshness_and_claim_no_author() {
        let connection = cover_database();
        let key = "artist|album".to_string();
        record_art_lookup(
            &connection,
            &key,
            ArtLookupOutcome::Found(&art_lookup(&key, "https://archive.org/front.jpg")),
        )
        .unwrap();

        // Age the answer past its retry stamp: the scanner should look again,
        // but a renderer still has the best picture this computer knows.
        let past = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        connection
            .execute("UPDATE album_art_lookups SET retry_at=?1", params![past])
            .unwrap();
        assert!(matches!(
            cached_art(&connection, &key).unwrap(),
            CachedArt::Unknown
        ));

        let covers = resolved_covers(&connection, std::slice::from_ref(&key)).unwrap();
        assert_eq!(covers.len(), 1);
        assert_eq!(covers[0].art, "https://archive.org/front.jpg");
        assert!(
            covers[0].author.is_empty() && covers[0].event_id.is_empty(),
            "a local resolution is not a claim by anybody"
        );

        // An album whose lookup found nothing has nothing to render.
        let empty = "artist|nowhere".to_string();
        record_art_lookup(&connection, &empty, ArtLookupOutcome::NoArt).unwrap();
        assert!(resolved_covers(&connection, &[empty]).unwrap().is_empty());
    }
}
