//! Finding album art for the music this computer knows about, and — when the
//! user allows it — publishing it as a kind `30427` claim.
//!
//! Why the desktop and not the phone: the cover NIP ranks a claim from an active
//! seeder of the album above anybody else's, and this computer is the seeder. It
//! also owns the relay pool and the signing key, and "no Nostr keys leave your
//! computer" is a property worth keeping. A phone inherits the result through
//! `NetworkService::best_known_covers`, so it needs no key and no HTTP client of
//! its own.
//!
//! Four rules shape everything here:
//!
//! * Two separate opt-ins. Asking MusicBrainz and the Cover Art Archive is a
//!   request to a central server; publishing is a signature made with a key the
//!   user owns. Somebody may reasonably want one, the other, or neither, so
//!   [`CoverPreferences`] keeps them apart and stores them for good.
//! * The switches *are* the control. [`CoverPublisher::start`] runs a worker
//!   that is woken when the library, the catalogue, or the switches change, so
//!   nothing has to be asked for album by album. A finished pass costs nothing
//!   until there is new music to look at.
//! * MusicBrainz asks for about one request a second and a user agent that says
//!   who is calling, so lookups are paced and identify themselves. When it
//!   answers 503 or 429 anyway the worker waits longer and carries on rather
//!   than hammering: the worst outcome here is being rude to a free service.
//! * Publishing is not paced and not capped. A relay takes claims as fast as
//!   they can be signed, so a pass sends the whole backlog.
//!
//! The queue is two things: every album this computer holds, and the albums a
//! window has actually drawn. The second is reported by the results pane as it
//! renders, and is deliberately *not* the catalogue cache — see
//! [`browsed_albums`].
//!
//! What the worker will not do is invent art. A release group is only used when
//! its title really matches the album, and a claim is only published when no
//! other author already has a winning one.

use crate::cover::{self, ArtLookup, CoverClaimFields};
use crate::network::NetworkService;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;

/// MusicBrainz requires a descriptive user agent and roughly one request a
/// second. Napstr says what it is rather than pretending to be a browser.
const USER_AGENT: &str = concat!(
    "Napstr/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/lnbits/napstr )"
);
/// One MusicBrainz request per album, paced. The Cover Art Archive is a
/// separate host, but the request before its own is always the MusicBrainz one,
/// so this is what keeps MusicBrainz inside its stated rate. Nothing paces the
/// *publishing*, which a relay will take as fast as it arrives.
const REQUEST_INTERVAL: Duration = Duration::from_millis(1200);
const HTTP_TIMEOUT: Duration = Duration::from_secs(12);
/// The first wait after a 503 or 429. Doubles with each consecutive refusal, so
/// a MusicBrainz outage costs a handful of requests rather than one per album.
const THROTTLE_BACKOFF_BASE: Duration = Duration::from_secs(30);
const THROTTLE_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// How many times the wait doubles before it stops growing.
const THROTTLE_BACKOFF_DOUBLINGS: u32 = 4;
/// A transient failure parks the album for this long before it is offered again.
const FAILED_LOOKUP_RETRY_SECONDS: i64 = 15 * 60;
/// The most albums the window may preview at once.
const MAX_PREVIEW: usize = 50;
/// A safety net rather than a cooldown: the worker is woken by real events, and
/// this only re-checks in case one was missed. A pass starts nothing unless
/// something is genuinely ready, so an idle library costs one indexed query.
const IDLE_RECHECK: Duration = Duration::from_secs(15 * 60);
/// Status is emitted at most this often while a pass runs, so a publishing
/// backlog of thousands cannot flood the window with events.
const REPORT_INTERVAL: Duration = Duration::from_millis(150);
const SETTING_LOOKUP_EXTERNAL: &str = "cover_lookup_external";
const SETTING_PUBLISH_CLAIMS: &str = "cover_publish_claims";
/// Emitted on every meaningful step, so the window shows the worker live.
pub const COVER_STATUS_EVENT: &str = "napstr-cover-status";

/// One album the worker can act on, and where it came from.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverCandidate {
    pub key: String,
    pub artist: String,
    pub album: String,
    pub track_count: usize,
    /// `library` for an album this computer holds, `catalogue` for one seen in
    /// somebody else's catalogue while browsing or searching.
    pub source: String,
}

/// An album a window is showing, as reported by the results pane.
///
/// The pane sends display metadata rather than a key, so the NIP's
/// normalization lives in exactly one place and cannot drift from the reader's.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverAlbumNote {
    pub artist: String,
    pub album: String,
}

/// The two things a user can switch on, kept apart on purpose: one sends a
/// question to a central server, the other signs with a key the user owns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoverPreferences {
    /// Ask MusicBrainz and the Cover Art Archive for art.
    pub lookup_external: bool,
    /// Sign and publish kind `30427` claims for art this computer resolved.
    pub publish_claims: bool,
}

impl CoverPreferences {
    fn any(&self) -> bool {
        self.lookup_external || self.publish_claims
    }

    fn describe(&self) -> String {
        match (self.lookup_external, self.publish_claims) {
            (false, false) => "Cover lookups and cover publishing are both off".into(),
            (true, false) => "Art is looked up automatically; nothing is signed".into(),
            (false, true) => "Only art already resolved here is signed and published".into(),
            (true, true) => "Art is looked up, then signed and published under your identity".into(),
        }
    }
}

/// What the worker is doing now, and what its last pass did.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverStatus {
    pub lookup_external: bool,
    pub publish_claims: bool,
    /// True while a pass is running.
    pub running: bool,
    /// The album being worked on.
    pub current: String,
    /// How many albums the current — or last — pass set out to do.
    pub pending: usize,
    pub remaining: usize,
    pub published: usize,
    /// Albums whose art was resolved here without being signed.
    pub resolved: usize,
    /// Albums somebody else had already covered, which this host leaves alone.
    pub already_covered: usize,
    /// Albums nobody has art for.
    pub no_art: usize,
    pub failed: usize,
    /// How many times a pass waited because MusicBrainz asked it to.
    pub backed_off: usize,
    /// True when a pass ended early because the switches were turned off.
    pub stopped: bool,
    pub message: String,
}

/// The background worker that keeps covers moving.
///
/// There is exactly one of these per process. It owns no timer beyond a safety
/// re-check: it is woken when something actually changes.
pub struct CoverPublisher {
    db_path: PathBuf,
    network: Arc<NetworkService>,
    app: AppHandle,
    /// Set while a pass is running, to make it stop at the next album.
    cancel: Arc<AtomicBool>,
    /// Woken by [`CoverPublisher::nudge`]; nudges coalesce.
    wake: Arc<Notify>,
    last_report: Mutex<Instant>,
    status: Mutex<CoverStatus>,
}

impl CoverPublisher {
    pub fn new(db_path: PathBuf, network: Arc<NetworkService>, app: AppHandle) -> Arc<Self> {
        // Preferences live in the database, so a choice made in an earlier
        // session is still in force in this one. An unreadable database means
        // "off": a privacy switch is never turned on by a failure.
        let preferences = read_preferences(&db_path).unwrap_or_default();
        Arc::new(Self {
            db_path,
            network,
            app,
            cancel: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            last_report: Mutex::new(Instant::now() - REPORT_INTERVAL),
            status: Mutex::new(CoverStatus {
                lookup_external: preferences.lookup_external,
                publish_claims: preferences.publish_claims,
                message: preferences.describe(),
                ..CoverStatus::default()
            }),
        })
    }

    /// Run the worker for the life of the process.
    ///
    /// It waits for a nudge, runs one pass over everything that is ready, then
    /// waits again. Because the switches are stored, the first nudge after a
    /// restart resumes the work — which is what lets a paired phone inherit art
    /// without doing any querying of its own.
    pub fn start(self: &Arc<Self>) {
        let publisher = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::select! {
                    _ = publisher.wake.notified() => {}
                    // A safety net rather than a cooldown: a pass starts nothing
                    // unless something is genuinely ready.
                    _ = tokio::time::sleep(IDLE_RECHECK) => {}
                }
                publisher.pass().await;
            }
        });
    }

    /// Ask for a pass. Cheap, idempotent, and safe to call wherever a trigger
    /// is noticed: a finished library scan, a search, a completed download.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    pub fn status(&self) -> CoverStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    pub fn preferences(&self) -> CoverPreferences {
        let status = self.status();
        CoverPreferences {
            lookup_external: status.lookup_external,
            publish_claims: status.publish_claims,
        }
    }

    /// Record the user's choice for good, then act on it: switching something on
    /// starts a pass at once, and switching everything off stops the pass that
    /// is running.
    pub fn set_preferences(&self, preferences: CoverPreferences) -> Result<CoverStatus, String> {
        {
            let connection = crate::open_connection(&self.db_path)?;
            for (key, enabled) in [
                (SETTING_LOOKUP_EXTERNAL, preferences.lookup_external),
                (SETTING_PUBLISH_CLAIMS, preferences.publish_claims),
            ] {
                connection
                    .execute(
                        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                        params![key, if enabled { "1" } else { "0" }],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        if let Ok(mut status) = self.status.lock() {
            status.lookup_external = preferences.lookup_external;
            status.publish_claims = preferences.publish_claims;
            status.stopped = false;
            status.message = preferences.describe();
        }
        if preferences.any() {
            self.cancel.store(false, Ordering::SeqCst);
            self.nudge();
        } else {
            // A stop mid-pass is deliberate: the user asked for nothing more to
            // leave this computer.
            self.cancel.store(true, Ordering::SeqCst);
        }
        Ok(self.report())
    }

    /// Ask a running pass to stop. It stops at the next album boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// What the worker would act on right now, capped for display.
    pub fn preview(&self, limit: usize) -> Result<Vec<CoverCandidate>, String> {
        let connection = crate::open_connection(&self.db_path)?;
        let mut pending = pending_albums(&connection, self.preferences())?;
        pending.truncate(limit.clamp(1, MAX_PREVIEW));
        Ok(pending)
    }

    /// Record the albums a window is showing, then wake the worker.
    ///
    /// This is how "albums seen in search results" reaches the queue: the pane
    /// reports what it draws, silently and with no button involved. Reporting
    /// costs one local write and is safe with both switches off.
    pub fn note_visible(&self, albums: &[CoverAlbumNote]) -> Result<usize, String> {
        let pairs = albums
            .iter()
            .map(|album| (album.artist.clone(), album.album.clone()))
            .collect::<Vec<_>>();
        let noted = {
            let connection = crate::open_connection(&self.db_path)?;
            cover::note_watched_albums(&connection, &pairs)?
        };
        if noted > 0 {
            self.nudge();
        }
        Ok(noted)
    }

    /// One pass: work through everything that is ready, then stop.
    ///
    /// A MusicBrainz 503 or 429 is not a failure of the album, it is a request
    /// to slow down. The pass waits — longer each time it is refused — and then
    /// carries on, because stopping would leave the library half-covered for the
    /// sake of somebody else's load spike.
    async fn pass(&self) {
        let preferences = self.preferences();
        if !preferences.any() {
            return;
        }
        let candidates = match crate::open_connection(&self.db_path).and_then(|connection| {
            // Entries a window reported long ago go before the list is built, so
            // the table cannot grow without bound.
            cover::prune_watch(&connection)?;
            pending_albums(&connection, preferences)
        }) {
            Ok(candidates) => candidates,
            Err(error) => {
                self.finish(format!("Could not work out what to look up: {error}"));
                return;
            }
        };
        if candidates.is_empty() {
            self.finish(String::new());
            return;
        }
        // Publishing needs the relay pool. Starting before it is up would only
        // walk the queue marking every album failed; the worker is nudged when
        // the network connects, so waiting costs nothing.
        if preferences.publish_claims
            && !self
                .network
                .status()
                .await
                .map(|status| status.connected)
                .unwrap_or(false)
        {
            self.finish("Waiting for Nostr before publishing".into());
            return;
        }
        let total = candidates.len();
        if let Ok(mut status) = self.status.lock() {
            status.running = true;
            status.stopped = false;
            status.pending = total;
            status.remaining = total;
            status.published = 0;
            status.resolved = 0;
            status.already_covered = 0;
            status.no_art = 0;
            status.failed = 0;
            status.backed_off = 0;
            status.current.clear();
            status.message = format!("Working through {total} albums");
        }
        self.cancel.store(false, Ordering::SeqCst);
        self.report();

        let client = match cover_http_client() {
            Ok(client) => client,
            Err(error) => {
                self.finish(format!("Could not prepare the cover lookup client: {error}"));
                return;
            }
        };

        let mut throttle_streak = 0u32;
        for candidate in &candidates {
            // The switches may have been turned off, or a stop asked for, while
            // this pass was running.
            if self.cancel.load(Ordering::SeqCst) || !self.preferences().any() {
                if let Ok(mut status) = self.status.lock() {
                    status.stopped = true;
                }
                break;
            }
            if let Ok(mut status) = self.status.lock() {
                status.current = format!("{} — {}", candidate.artist, candidate.album);
                status.remaining = status.remaining.saturating_sub(1);
            }
            self.tick();
            // MusicBrainz's rate limit is the reason this loop is slow, and it
            // is the only thing here that is paced: publishing is not.
            tokio::time::sleep(REQUEST_INTERVAL).await;
            match self.consider(&client, candidate, preferences).await {
                Ok(considered) => {
                    throttle_streak = 0;
                    match considered {
                        Considered::Published => self.tally(|status| status.published += 1),
                        Considered::Resolved => self.tally(|status| status.resolved += 1),
                        Considered::AlreadyCovered => {
                            self.tally(|status| status.already_covered += 1)
                        }
                        Considered::NoArt => self.tally(|status| status.no_art += 1),
                    }
                }
                Err(LookupError::Throttled { retry_after }) => {
                    throttle_streak += 1;
                    let delay = throttle_delay(throttle_streak, retry_after);
                    // Remember the pause, so a later pass does not walk straight
                    // back into the same refusal.
                    self.park(&candidate.key, delay);
                    if let Ok(mut status) = self.status.lock() {
                        status.backed_off += 1;
                        status.message = format!(
                            "MusicBrainz asked Napstr to slow down; waiting {}s",
                            delay.as_secs()
                        );
                    }
                    self.report();
                    tokio::time::sleep(delay).await;
                }
                Err(LookupError::Failed(message)) => {
                    // A transient fault is not worth retrying immediately, so
                    // the album is parked for a short while.
                    self.park(
                        &candidate.key,
                        Duration::from_secs(FAILED_LOOKUP_RETRY_SECONDS as u64),
                    );
                    if let Ok(mut status) = self.status.lock() {
                        status.failed += 1;
                        status.message = message;
                    }
                }
            }
            self.tick();
        }
        self.finish(String::new());
    }

    /// End a pass: state what happened, and say plainly when nothing was ready.
    fn finish(&self, failure: String) {
        if let Ok(mut status) = self.status.lock() {
            status.running = false;
            status.current.clear();
            status.message = if !failure.is_empty() {
                failure
            } else if status.pending == 0 {
                "Nothing to do right now".into()
            } else if status.stopped {
                format!(
                    "Stopped with {} of {} albums left; switching covers back on resumes",
                    status.remaining, status.pending
                )
            } else if status.backed_off > 0 {
                format!(
                    "{} · MusicBrainz asked Napstr to slow down {} time(s)",
                    summarize(&status),
                    status.backed_off
                )
            } else {
                summarize(&status)
            };
        }
        self.report();
    }

    fn tally(&self, change: impl FnOnce(&mut CoverStatus)) {
        if let Ok(mut status) = self.status.lock() {
            change(&mut status);
        }
    }

    /// One album: use the cache, resolve when allowed, then publish.
    async fn consider(
        &self,
        client: &reqwest::Client,
        candidate: &CoverCandidate,
        preferences: CoverPreferences,
    ) -> Result<Considered, LookupError> {
        let resolution = match self.cached_art(&candidate.key)? {
            // A resolution made earlier — or by this very pass — is not worth a
            // second MusicBrainz request.
            cover::CachedArt::Found(resolution) => *resolution,
            // Asked recently and answered "none", or parked after a failure.
            cover::CachedArt::Suppressed => return Ok(Considered::NoArt),
            // Lookups are off, so the only art this pass may use is art the
            // computer already holds. The pending list already filtered on this,
            // so reaching the guard means the answer changed underneath us.
            cover::CachedArt::Unknown if !preferences.lookup_external => {
                match self.stored_art(&candidate.key)? {
                    Some(resolution) => resolution,
                    None => return Ok(Considered::NoArt),
                }
            }
            cover::CachedArt::Unknown => {
                // A relay may have answered this album since the pass began, and
                // somebody else's claim is not this host's to overwrite — nor is
                // it worth a lookup to duplicate.
                if !self
                    .network
                    .album_covers(vec![candidate.key.clone()])
                    .await
                    .map_err(LookupError::Failed)?
                    .is_empty()
                {
                    return Ok(Considered::AlreadyCovered);
                }
                match resolve(client, candidate).await? {
                    Some(resolution) => {
                        self.record(&candidate.key, Some(&resolution))?;
                        resolution
                    }
                    // MusicBrainz has nothing today. If this computer already
                    // holds art for the album, keep it: one unhelpful answer is
                    // no reason to throw away a working picture.
                    None => match self.stored_art(&candidate.key)? {
                        Some(previous) => {
                            self.record(&candidate.key, Some(&previous))?;
                            previous
                        }
                        None => {
                            self.record(&candidate.key, None)?;
                            return Ok(Considered::NoArt);
                        }
                    },
                }
            }
        };
        if !preferences.publish_claims {
            return Ok(Considered::Resolved);
        }
        // Nothing is paced or capped here: a relay takes a claim as fast as it
        // can be signed, and the pass sends the whole backlog.
        let fields = CoverClaimFields {
            key: resolution.key,
            art: resolution.art,
            thumb: resolution.thumb,
            mbid: resolution.mbid,
            year: resolution.year,
            genre: String::new(),
            collection: resolution.collection,
            source: resolution.source,
        };
        self.network
            .publish_cover(fields)
            .await
            .map_err(LookupError::Failed)?;
        Ok(Considered::Published)
    }

    fn cached_art(&self, key: &str) -> Result<cover::CachedArt, LookupError> {
        let connection = crate::open_connection(&self.db_path).map_err(LookupError::Failed)?;
        cover::cached_art(&connection, key).map_err(LookupError::Failed)
    }

    /// Art this computer holds for an album, whatever its age. Freshness decides
    /// whether MusicBrainz is asked again; it never withholds a picture that is
    /// already here.
    fn stored_art(&self, key: &str) -> Result<Option<ArtLookup>, LookupError> {
        let connection = crate::open_connection(&self.db_path).map_err(LookupError::Failed)?;
        cover::stored_art(&connection, key).map_err(LookupError::Failed)
    }

    fn record(&self, key: &str, resolution: Option<&ArtLookup>) -> Result<(), LookupError> {
        let connection = crate::open_connection(&self.db_path).map_err(LookupError::Failed)?;
        let outcome = match resolution {
            Some(resolution) => cover::ArtLookupOutcome::Found(resolution),
            None => cover::ArtLookupOutcome::NoArt,
        };
        cover::record_art_lookup(&connection, key, outcome).map_err(LookupError::Failed)
    }

    /// Park an album that could not be resolved for `delay`, best effort.
    fn park(&self, key: &str, delay: Duration) {
        let Ok(connection) = crate::open_connection(&self.db_path) else {
            return;
        };
        let _ = cover::record_art_lookup(
            &connection,
            key,
            cover::ArtLookupOutcome::Failed {
                retry_after_seconds: delay.as_secs().min(i64::MAX as u64) as i64,
            },
        );
    }

    /// Emit the status now. Used for anything the window must not miss.
    fn report(&self) -> CoverStatus {
        let status = self.status();
        if let Ok(mut last) = self.last_report.lock() {
            *last = Instant::now();
        }
        let _ = self.app.emit(COVER_STATUS_EVENT, status.clone());
        status
    }

    /// Emit the status, at most every [`REPORT_INTERVAL`]. A backlog of
    /// thousands must not flood the window with events.
    fn tick(&self) {
        let now = Instant::now();
        let due = self
            .last_report
            .lock()
            .map(|mut last| {
                if now.duration_since(*last) >= REPORT_INTERVAL {
                    *last = now;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if due {
            let _ = self.app.emit(COVER_STATUS_EVENT, self.status());
        }
    }
}

/// What one album produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Considered {
    /// Signed and filed under the user's identity.
    Published,
    /// Art known to this computer, deliberately not signed.
    Resolved,
    /// Somebody else's claim already answers this album.
    AlreadyCovered,
    /// Nobody has art for this record.
    NoArt,
}

/// Why one album could not be resolved.
#[derive(Debug)]
enum LookupError {
    /// MusicBrainz or the archive asked this client to slow down.
    Throttled { retry_after: Option<Duration> },
    /// Anything else that stopped this album being resolved.
    Failed(String),
}

/// How an HTTP status should be read.
enum Answer {
    Success,
    /// The archive's ordinary "nobody has scanned this record".
    NotFound,
    Throttled { retry_after: Option<Duration> },
    Failed(String),
}

/// `not_found_is_answer` is true where a 404 really means "no art exists", and
/// false where it means the endpoint has moved and should not be remembered as
/// an album with no cover.
fn classify(
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
    host: &str,
    not_found_is_answer: bool,
) -> Answer {
    if status.is_success() {
        return Answer::Success;
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return if not_found_is_answer {
            Answer::NotFound
        } else {
            Answer::Failed(format!("{host} answered {status}"))
        };
    }
    // 503 is MusicBrainz's own back-pressure; 429 is the standard one.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Answer::Throttled {
            retry_after: retry_after.and_then(parse_retry_after),
        };
    }
    Answer::Failed(format!("{host} answered {status}"))
}

/// `Retry-After` is either delta-seconds or an HTTP date. Only the numeric form
/// is honoured; a date is treated as "no hint" and the exponential floor wins.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<u64>().ok()?;
    (seconds > 0).then(|| Duration::from_secs(seconds).min(THROTTLE_BACKOFF_MAX))
}

/// How long to wait after `streak` consecutive refusals, honouring the server's
/// own hint when it asks for longer than the exponential floor.
fn throttle_delay(streak: u32, retry_after: Option<Duration>) -> Duration {
    let step = streak.saturating_sub(1).min(THROTTLE_BACKOFF_DOUBLINGS);
    let backoff = (THROTTLE_BACKOFF_BASE * 2u32.pow(step)).min(THROTTLE_BACKOFF_MAX);
    retry_after.map_or(backoff, |hint| hint.clamp(backoff, THROTTLE_BACKOFF_MAX))
}

fn retry_after_header(response: &reqwest::Response) -> Option<&str> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
}

/// The line the window shows when a pass ends.
fn summarize(status: &CoverStatus) -> String {
    let mut parts = Vec::new();
    if status.published > 0 {
        parts.push(format!("published {}", status.published));
    }
    if status.resolved > 0 {
        parts.push(format!("resolved {}", status.resolved));
    }
    if status.already_covered > 0 {
        parts.push(format!("already covered {}", status.already_covered));
    }
    if status.no_art > 0 {
        parts.push(format!("no art anywhere {}", status.no_art));
    }
    if status.failed > 0 {
        parts.push(format!("failed {}", status.failed));
    }
    if parts.is_empty() {
        return "Nothing changed".into();
    }
    let verb = if status.publish_claims {
        "Finished"
    } else {
        "Finished (nothing signed)"
    };
    format!("{verb}: {}", parts.join(", "))
}

#[derive(Deserialize)]
struct MusicBrainzSearch {
    #[serde(default, rename = "release-groups")]
    release_groups: Vec<MusicBrainzGroup>,
}

#[derive(Deserialize, Clone)]
struct MusicBrainzGroup {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "first-release-date")]
    first_release_date: String,
    #[serde(default, rename = "primary-type")]
    primary_type: String,
    /// MusicBrainz's own search rank. Shown to a person choosing by hand, and
    /// deliberately never trusted for the automatic choice.
    #[serde(default)]
    score: u32,
    #[serde(default, rename = "secondary-types")]
    secondary_types: Vec<String>,
}

/// Pick the release group that really is this album.
///
/// A matching title is not enough. `St. Anger` the album, the EP and the single
/// all share one, and MusicBrainz ranks them by search score rather than by what
/// a listener means — a single's Cover Art Archive entry is usually empty, so
/// choosing one turns a record that has art into "no art anywhere" for a
/// fortnight. A music library means the album, so an album group wins over
/// anything else whose title matches.
fn best_release_group<'a>(
    groups: &'a [MusicBrainzGroup],
    album: &str,
) -> Option<&'a MusicBrainzGroup> {
    groups
        .iter()
        .filter(|group| !group.id.is_empty() && alike(&group.title, album))
        .min_by_key(|group| {
            (
                // A music library means the album, not the seven-inch single.
                u8::from(!group.primary_type.eq_ignore_ascii_case("album")),
                // An exact title beats one that merely contains it, so
                // `St. Anger` wins over `St. Anger Live Rarities`.
                u8::from(fold_title(&group.title) != fold_title(album)),
            )
        })
}

#[derive(Deserialize)]
struct CoverArtArchive {
    #[serde(default)]
    images: Vec<CoverArtImage>,
}

#[derive(Deserialize, Default)]
struct CoverArtImage {
    #[serde(default)]
    image: String,
    #[serde(default)]
    front: bool,
    #[serde(default)]
    thumbnails: CoverArtThumbnails,
}

#[derive(Deserialize, Default)]
struct CoverArtThumbnails {
    #[serde(default)]
    small: String,
    /// The archive's 1200-pixel rendition. Asked for by number because the
    /// archive's `large` alias means 500, which is too soft for an album header.
    #[serde(default, rename = "1200")]
    full: String,
}

/// Ask MusicBrainz for the release group, then the Cover Art Archive for its
/// front image. `Ok(None)` is a considered answer: this album has no art there.
///
/// A 503 or 429 comes back as [`LookupError::Throttled`] rather than a plain
/// failure, because the caller's correct response is to wait, not to give up or
/// to try the next album immediately.
async fn resolve(
    client: &reqwest::Client,
    candidate: &CoverCandidate,
) -> Result<Option<ArtLookup>, LookupError> {
    let query = default_query(&candidate.artist, &candidate.album);
    // Encoded through `Url` rather than `RequestBuilder::query`, which reqwest
    // 0.13 puts behind its `query` feature. This needs no extra dependency and
    // keeps the desktop's reqwest features identical to the companion's.
    let mut search_url = reqwest::Url::parse("https://musicbrainz.org/ws/2/release-group/")
        .map_err(|error| LookupError::Failed(format!("could not build the MusicBrainz query: {error}")))?;
    search_url
        .query_pairs_mut()
        .append_pair("query", &query)
        .append_pair("fmt", "json")
        .append_pair("limit", "10");
    let search = client
        .get(search_url)
        .send()
        .await
        .map_err(|error| LookupError::Failed(format!("MusicBrainz lookup failed: {error}")))?;
    match classify(
        search.status(),
        retry_after_header(&search),
        "MusicBrainz",
        // A 404 from MusicBrainz means the endpoint moved, not that this album
        // has no art, so it must not be remembered as a considered answer.
        false,
    ) {
        Answer::Success => {}
        Answer::NotFound => return Ok(None),
        Answer::Throttled { retry_after } => return Err(LookupError::Throttled { retry_after }),
        Answer::Failed(message) => return Err(LookupError::Failed(message)),
    }
    let found: MusicBrainzSearch = search
        .json()
        .await
        .map_err(|error| LookupError::Failed(format!("MusicBrainz sent something unreadable: {error}")))?;
    // Only a release group whose title really is this album is worth publishing:
    // a cover on the wrong record is worse than a blank square. Among those,
    // the album itself is the one a music library means.
    let Some(group) = best_release_group(&found.release_groups, &candidate.album).cloned() else {
        return Ok(None);
    };

    let Some((art, thumb, _)) = archive_lookup(client, &group.id).await? else {
        return Ok(None);
    };
    Ok(Some(ArtLookup {
        key: candidate.key.clone(),
        art,
        thumb,
        mbid: group.id,
        year: group.first_release_date.chars().take(4).collect(),
        collection: group.title,
        source: "musicbrainz".into(),
    }))
}

/// The MusicBrainz query Napstr asks for an album.
///
/// Also where the manual art tool starts, so a person editing a search sees
/// exactly what the automatic lookup sent rather than a blank box.
pub(crate) fn default_query(artist: &str, album: &str) -> String {
    format!(
        "release:\"{}\" AND artist:\"{}\"",
        escape_query(album),
        escape_query(artist)
    )
}

/// The client every cover lookup uses: the user agent MusicBrainz asks for, and
/// a timeout so one stalled answer cannot hold up a pass or a person.
fn cover_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| format!("Could not prepare the cover lookup client: {error}"))
}

/// An image URL the NIP will accept, upgrading the archive's own `http://` links.
///
/// The Cover Art Archive is inconsistent about the scheme: the identical request
/// answers `https://` for one release group and `http://` for another (measured:
/// `8f1cc89b-7e80-3e1c-b571-8cf2b98db347` https, `e58ba6c6-7e54-461f-aa61-403f60c1b188`
/// http, both with a front image). The same host and path work over TLS, which is
/// why a browser pointed at `/front` shows the art.
///
/// A cover claim must carry an HTTPS URL, but discarding the image — which is
/// what this used to do — turns a record that has art into "no art anywhere",
/// and caches that lie for a fortnight. So the scheme is repaired for hosts that
/// are known to serve the same path over TLS, and anything else is still
/// refused.
fn secure_image_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("https://") {
        return Some(trimmed.to_string());
    }
    let rest = trimmed.strip_prefix("http://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop any userinfo, then any port, leaving just the host.
    let host = authority.split('@').next_back().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let serves_tls = host == "coverartarchive.org"
        || host.ends_with(".coverartarchive.org")
        || host == "archive.org"
        || host.ends_with(".archive.org");
    serves_tls.then(|| format!("https://{rest}"))
}

/// The front image of an archive answer, as `(art, thumb, is_front)`.
///
/// A group with no `front` flag still has something worth offering a person, but
/// the automatic lookup must not quietly publish a back cover, so the caller is
/// told which of the two it got.
fn front_image(archive: &CoverArtArchive) -> Option<(String, String, bool)> {
    let front = archive
        .images
        .iter()
        .find(|image| image.front && !image.image.is_empty());
    let chosen = front.or_else(|| archive.images.iter().find(|image| !image.image.is_empty()))?;
    // The archive serves back the file that was uploaded, which is routinely far
    // bigger than anything draws it: a phone album header is around 750 device
    // pixels, so the 1200-pixel rendition is the whole picture at a fraction of
    // the bytes. An upload smaller than that has no 1200 rendition, and the
    // original is then already the smaller file.
    let art = secure_image_url(&chosen.thumbnails.full).or_else(|| secure_image_url(&chosen.image))?;
    // A thumbnail is a convenience: one that is missing or unusable must not
    // cost the cover itself.
    let thumb = secure_image_url(&chosen.thumbnails.small).unwrap_or_default();
    Some((art, thumb, front.is_some()))
}

/// Ask the Cover Art Archive what art one release group has.
async fn archive_lookup(
    client: &reqwest::Client,
    mbid: &str,
) -> Result<Option<(String, String, bool)>, LookupError> {
    let response = client
        .get(format!(
            "https://coverartarchive.org/release-group/{mbid}"
        ))
        .send()
        .await
        .map_err(|error| LookupError::Failed(format!("Cover Art Archive lookup failed: {error}")))?;
    match classify(
        response.status(),
        retry_after_header(&response),
        "Cover Art Archive",
        // Here a 404 really is the ordinary "nobody has scanned this record".
        true,
    ) {
        Answer::Success => {}
        Answer::NotFound => return Ok(None),
        Answer::Throttled { retry_after } => return Err(LookupError::Throttled { retry_after }),
        Answer::Failed(message) => return Err(LookupError::Failed(message)),
    }
    let archive: CoverArtArchive = response.json().await.map_err(|error| {
        LookupError::Failed(format!("Cover Art Archive sent something unreadable: {error}"))
    })?;
    Ok(front_image(&archive))
}

/// Ask MusicBrainz for the release groups one query matches.
async fn search_groups(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<MusicBrainzGroup>, LookupError> {
    let mut url = reqwest::Url::parse("https://musicbrainz.org/ws/2/release-group/")
        .map_err(|error| LookupError::Failed(format!("could not build the MusicBrainz query: {error}")))?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("fmt", "json")
        .append_pair("limit", "10");
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| LookupError::Failed(format!("MusicBrainz lookup failed: {error}")))?;
    match classify(
        response.status(),
        retry_after_header(&response),
        "MusicBrainz",
        false,
    ) {
        Answer::Success => {}
        Answer::NotFound => return Ok(Vec::new()),
        Answer::Throttled { retry_after } => return Err(LookupError::Throttled { retry_after }),
        Answer::Failed(message) => return Err(LookupError::Failed(message)),
    }
    let found: MusicBrainzSearch = response.json().await.map_err(|error| {
        LookupError::Failed(format!("MusicBrainz sent something unreadable: {error}"))
    })?;
    Ok(found.release_groups)
}

/// Turn a lookup failure into something worth putting in front of a person.
fn describe_lookup_error(error: LookupError) -> String {
    match error {
        LookupError::Failed(message) => message,
        LookupError::Throttled { retry_after } => match retry_after {
            Some(delay) => format!(
                "MusicBrainz asked Napstr to slow down \u{2014} try again in about {} seconds",
                delay.as_secs().max(1)
            ),
            None => "MusicBrainz asked Napstr to slow down \u{2014} try again in a moment".into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Choosing art by hand
// ---------------------------------------------------------------------------

/// One release group MusicBrainz offered, with the art the archive holds for it.
///
/// The automatic choice ranks by primary type and an exact title, which is right
/// far more often than not but cannot know that a person meant a different
/// pressing. Showing the candidates beside their art settles that in one look.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverSearchHit {
    pub mbid: String,
    pub title: String,
    /// The primary type, with any `Live`/`Compilation` markers after it.
    pub types: String,
    pub year: String,
    pub score: u32,
    pub art: String,
    pub thumb: String,
    /// True when the archive's own front image was used.
    pub front: bool,
    /// True for the group the automatic lookup would have picked.
    pub chosen: bool,
}

/// Art a person chose for an album, replacing whatever was there before.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverManualPick {
    pub artist: String,
    pub album: String,
    pub mbid: String,
    pub art: String,
    pub thumb: String,
    /// The release group title, published as `collection`.
    pub title: String,
    pub year: String,
}

/// What applying a pick did.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverPickResult {
    /// The `d` value used: the key the automatic lookup computes for the same
    /// album, so a later lookup finds this claim again.
    pub key: String,
    pub published: bool,
    pub event_id: String,
    /// Why nothing was signed, when nothing was.
    pub note: String,
}

impl CoverPublisher {
    /// Release groups for one album, each with whatever art the archive has.
    ///
    /// `query` is a person's own edited Lucene query; `None` uses the query the
    /// automatic lookup sends, so the tool starts from what Napstr already asked.
    pub async fn search_candidates(
        &self,
        artist: &str,
        album: &str,
        query: Option<String>,
    ) -> Result<Vec<CoverSearchHit>, String> {
        let query = query
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_query(artist, album));
        let client = cover_http_client()?;
        let groups = search_groups(&client, &query)
            .await
            .map_err(describe_lookup_error)?;
        let chosen = best_release_group(&groups, album).map(|group| group.id.clone());
        let mut hits = Vec::new();
        for group in groups {
            if group.id.is_empty() {
                continue;
            }
            let (art, thumb, front) = match archive_lookup(&client, &group.id).await {
                Ok(Some(found)) => found,
                // A group with no art is still worth showing: it is the answer to
                // "why does this album keep coming back empty".
                Ok(None) => (String::new(), String::new(), false),
                Err(error) => {
                    // Whatever arrived before the throttle is still usable, and
                    // better than throwing away the search just run.
                    if hits.is_empty() {
                        return Err(describe_lookup_error(error));
                    }
                    break;
                }
            };
            let mut types = group.primary_type.clone();
            for extra in &group.secondary_types {
                if !extra.is_empty() {
                    types = format!("{types} \u{b7} {extra}");
                }
            }
            hits.push(CoverSearchHit {
                chosen: chosen.as_deref() == Some(group.id.as_str()),
                mbid: group.id,
                title: group.title,
                types: types
                    .trim_matches(|character: char| character == ' ' || character == '\u{b7}')
                    .to_string(),
                year: group.first_release_date.chars().take(4).collect(),
                score: group.score,
                art,
                thumb,
                front,
            });
        }
        Ok(hits)
    }

    /// File the art a person picked, and sign it if publishing is switched on.
    ///
    /// The key is computed from display metadata exactly as the automatic lookup
    /// computes it, so the `d` tag matches what a lookup would have produced and
    /// the claim is found again by anybody filtering on that key.
    pub async fn apply_pick(&self, pick: CoverManualPick) -> Result<CoverPickResult, String> {
        let key = cover::cover_key(&pick.artist, &pick.album)
            .ok_or("this album has no addressable cover key")?;
        let art = pick.art.trim().to_string();
        if !art.starts_with("https://") {
            return Err("a cover needs an HTTPS image URL".into());
        }
        let lookup = ArtLookup {
            key: key.clone(),
            art: art.clone(),
            thumb: pick.thumb.trim().to_string(),
            mbid: pick.mbid.trim().to_string(),
            year: pick.year.trim().chars().take(4).collect(),
            collection: pick.title.trim().to_string(),
            source: "manual".into(),
        };
        {
            // Recording it as a resolution is what makes the choice appear at
            // once and survive a restart, whether or not anything is published.
            let connection = crate::open_connection(&self.db_path)?;
            cover::record_art_lookup(&connection, &key, cover::ArtLookupOutcome::Found(&lookup))?;
        }
        self.report();
        let mut result = CoverPickResult {
            key: key.clone(),
            ..CoverPickResult::default()
        };
        if !self.preferences().publish_claims {
            result.note =
                "Saved on this computer. Switch on publishing to sign it as a kind 30427 claim."
                    .into();
            return Ok(result);
        }
        match self
            .network
            .publish_cover(cover::CoverClaimFields {
                key,
                art,
                thumb: lookup.thumb,
                mbid: lookup.mbid,
                year: lookup.year,
                genre: String::new(),
                collection: lookup.collection,
                source: lookup.source,
            })
            .await
        {
            Ok(event_id) => {
                result.published = true;
                result.event_id = event_id;
                result.note = "Signed and published to your relays.".into();
            }
            Err(error) => {
                result.note = format!("Saved on this computer, but publishing failed: {error}");
            }
        }
        Ok(result)
    }
}

/// MusicBrainz's Lucene syntax treats these as operators.
fn escape_query(value: &str) -> String {
    value
        .chars()
        .filter(|character| !"\"\\()[]{}^~*?:".contains(*character))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Compare album names the way a person would: case, punctuation and spacing do
/// not distinguish one record from another.
/// Album names the way a person reads them: case, punctuation and spacing do
/// not distinguish one record from another.
fn fold_title(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn alike(left: &str, right: &str) -> bool {
    let left = fold_title(left);
    let right = fold_title(right);
    !left.is_empty() && !right.is_empty() && (left == right || left.contains(&right) || right.contains(&left))
}

/// Read the user's cover choices.
///
/// Any failure — no database yet, no row yet — reads as "off". A privacy
/// switch is never turned on by an error.
fn read_preferences(db_path: &Path) -> Result<CoverPreferences, String> {
    let connection = crate::open_connection(db_path)?;
    Ok(CoverPreferences {
        lookup_external: read_flag(&connection, SETTING_LOOKUP_EXTERNAL),
        publish_claims: read_flag(&connection, SETTING_PUBLISH_CLAIMS),
    })
}

fn read_flag(connection: &rusqlite::Connection, key: &str) -> bool {
    connection
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// Albums this computer holds, grouped the way a cover is addressed.
fn library_albums(
    connection: &rusqlite::Connection,
) -> Result<Vec<(String, String, String, usize, &'static str)>, String> {
    let mut statement = connection
        .prepare(
            "SELECT artist, album, COUNT(*) FROM files
             WHERE format IN ('MP3','FLAC','WAV','OGG','OPUS')
               AND TRIM(artist) <> '' AND TRIM(album) <> ''
               AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)
             GROUP BY artist, album
             ORDER BY artist, album",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut albums = Vec::new();
    for row in rows {
        let (artist, album, track_count) = row.map_err(|error| error.to_string())?;
        let Some(key) = cover::cover_key(&artist, &album) else {
            continue;
        };
        albums.push((key, artist, album, track_count.max(0) as usize, "library"));
    }
    Ok(albums)
}

/// Albums a window actually put on screen, which is what "albums seen in search
/// results" means.
///
/// The source is [`cover::watched_albums`] — the albums a window reported
/// drawing — and deliberately *not* `remote_catalogue`. That table holds every
/// album in every catalogue ever searched: measured on a real install it was
/// 3,684 albums against 4 in the library, 71% of them cached weeks earlier. A
/// queue built from it would spend hours asking MusicBrainz about records this
/// computer does not own.
fn browsed_albums(
    connection: &rusqlite::Connection,
) -> Result<Vec<(String, String, String, usize, &'static str)>, String> {
    Ok(cover::watched_albums(connection)?
        .into_iter()
        .map(|(key, artist, album)| (key, artist, album, 0usize, "browsed"))
        .collect())
}

/// Everything the worker can act on right now, this computer's own albums
/// first, deduplicated by cover key.
fn pending_albums(
    connection: &rusqlite::Connection,
    preferences: CoverPreferences,
) -> Result<Vec<CoverCandidate>, String> {
    let covered = stored_cover_keys(connection)?;
    // With lookups switched off the only useful work is signing art this
    // computer already holds, so anything else would fill the pending list with
    // albums that cannot progress.
    let held = if preferences.lookup_external {
        None
    } else {
        Some(cover::stored_art_keys(connection)?)
    };
    let mut albums = library_albums(connection)?;
    albums.extend(browsed_albums(connection)?);
    let keys = albums
        .iter()
        .map(|(key, ..)| key.clone())
        .collect::<Vec<_>>();
    let suppressed = if preferences.lookup_external {
        // Fresh "no art" answers and parked failures: asking again would only
        // spend a request to learn the same thing.
        cover::suppressed_art_keys(connection, &keys)?
    } else {
        HashSet::new()
    };
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for (key, artist, album, track_count, source) in albums {
        if !seen.insert(key.clone()) {
            continue;
        }
        // A claim filed under another selector of the same album counts.
        if cover::cover_lookup_keys(&key)
            .iter()
            .any(|selector| covered.contains(selector))
        {
            continue;
        }
        if suppressed.contains(&key) {
            continue;
        }
        if held.as_ref().is_some_and(|held| !held.contains(&key)) {
            continue;
        }
        candidates.push(CoverCandidate {
            key,
            artist,
            album,
            track_count,
            source: source.to_string(),
        });
    }
    Ok(candidates)
}

/// Cover keys that already have a live claim from somebody.
fn stored_cover_keys(
    connection: &rusqlite::Connection,
) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare("SELECT DISTINCT cover_key FROM album_covers WHERE deleted=0")
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
    // Only the retention test reads the clock; the worker itself never does.
    use chrono::Utc;

    /// Mirrors `initialise_database`: the main schema owns the settings, the
    /// library and the block lists, the network schema owns the catalogue and
    /// the cover tables.
    fn cover_database() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE blocked_pubkeys (pubkey TEXT PRIMARY KEY, reason TEXT NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE blocked_files (file_id TEXT PRIMARY KEY, reason TEXT NOT NULL, created_at TEXT NOT NULL);
                 CREATE TABLE files (
                   file_id TEXT PRIMARY KEY, filename TEXT NOT NULL, path TEXT NOT NULL, size INTEGER NOT NULL,
                   format TEXT NOT NULL, indexed_at TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
                   artist TEXT NOT NULL DEFAULT '', album TEXT NOT NULL DEFAULT '',
                   mime TEXT NOT NULL DEFAULT '', license TEXT NOT NULL DEFAULT '',
                   description TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '',
                   folder TEXT NOT NULL DEFAULT '', modified_ns INTEGER NOT NULL DEFAULT 0,
                   track_number INTEGER NOT NULL DEFAULT 0, disc_number INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
        crate::network::initialise_network_schema(&connection).unwrap();
        connection
    }

    fn insert_library_track(connection: &rusqlite::Connection, id: &str, artist: &str, album: &str) {
        connection
            .execute(
                "INSERT INTO files(file_id,filename,path,size,format,indexed_at,artist,album)
                 VALUES(?1,?2,?2,1,'MP3','now',?3,?4)",
                params![id, format!("{id}.mp3"), artist, album],
            )
            .unwrap();
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
    fn rate_limiting_is_backpressure_rather_than_failure() {
        match classify(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Some("90"),
            "MusicBrainz",
            false,
        ) {
            Answer::Throttled { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(90)));
            }
            _ => panic!("a 503 from MusicBrainz is a request to slow down, not a failure"),
        }
        assert!(matches!(
            classify(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                None,
                "MusicBrainz",
                false
            ),
            Answer::Throttled { retry_after: None }
        ));

        // A 404 is a considered answer for the archive and a fault for
        // MusicBrainz, where it means the endpoint moved.
        assert!(matches!(
            classify(
                reqwest::StatusCode::NOT_FOUND,
                None,
                "Cover Art Archive",
                true
            ),
            Answer::NotFound
        ));
        assert!(matches!(
            classify(reqwest::StatusCode::NOT_FOUND, None, "MusicBrainz", false),
            Answer::Failed(_)
        ));
        // A real client error is a failure, not back-pressure.
        assert!(matches!(
            classify(reqwest::StatusCode::BAD_REQUEST, None, "MusicBrainz", false),
            Answer::Failed(_)
        ));
        assert!(matches!(
            classify(reqwest::StatusCode::OK, None, "MusicBrainz", false),
            Answer::Success
        ));
    }

    #[test]
    fn backoff_grows_and_respects_a_retry_after_hint() {
        assert_eq!(throttle_delay(1, None), Duration::from_secs(30));
        assert_eq!(throttle_delay(2, None), Duration::from_secs(60));
        assert_eq!(throttle_delay(3, None), Duration::from_secs(120));
        // It stops growing rather than turning a relay's bad afternoon into a
        // scan that never finishes.
        assert_eq!(throttle_delay(9, None), THROTTLE_BACKOFF_MAX);
        // The server's own hint wins when it asks for longer, and is capped
        // when it is absurd.
        assert_eq!(
            throttle_delay(1, Some(Duration::from_secs(120))),
            Duration::from_secs(120)
        );
        assert_eq!(
            throttle_delay(9, Some(Duration::from_secs(10))),
            THROTTLE_BACKOFF_MAX
        );

        assert_eq!(parse_retry_after(" 45 "), Some(Duration::from_secs(45)));
        assert_eq!(parse_retry_after("0"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    #[test]
    fn pending_is_the_library_plus_what_a_window_showed() {
        let connection = cover_database();
        insert_library_track(&connection, "aa", "Artist", "Album");
        // The same album shown in a window is one candidate, not two, and the
        // library entry is the one that survives.
        cover::note_watched_albums(&connection, &[("Artist".to_string(), "Album".to_string())])
            .unwrap();
        // One album somebody else already covers, and one answered "no art"
        // a moment ago: neither is work.
        insert_library_track(&connection, "dd", "Other", "Record");
        insert_library_track(&connection, "ee", "Quiet", "Record");
        connection
            .execute(
                "INSERT INTO album_covers(cover_key,source_pubkey,art,thumb,mbid,year,genre,collection,source,cover_file_id,mime,event_id,created_at,deleted,seeder,seen_at)
                 VALUES('other|record','aa','https://example.com/a.jpg','','','','','','itunes','','','bb',1,0,0,'now')",
                [],
            )
            .unwrap();
        cover::record_art_lookup(&connection, "quiet|record", cover::ArtLookupOutcome::NoArt)
            .unwrap();

        let lookups_on = || CoverPreferences {
            lookup_external: true,
            publish_claims: false,
        };
        let pending = pending_albums(&connection, lookups_on()).unwrap();
        assert_eq!(pending.len(), 1, "only the album with real work left is pending");
        assert_eq!(pending[0].key, "artist|album");
        assert_eq!(pending[0].source, "library");
        assert_eq!(pending[0].track_count, 1);

        // An album a window is showing is work too, labelled as browsed.
        cover::note_watched_albums(&connection, &[("Browsing".to_string(), "Now".to_string())])
            .unwrap();
        let pending = pending_albums(&connection, lookups_on()).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[1].key, "browsing|now");
        assert_eq!(pending[1].source, "browsed");

        // An entry that has aged out is not a queue entry any more, and pruning
        // removes it. This is retention rather than a freshness heuristic: a
        // long pass must not lose an album that is still on screen.
        connection
            .execute(
                "UPDATE cover_watch SET noted_at=?1 WHERE cover_key='browsing|now'",
                params![(Utc::now() - chrono::Duration::hours(48)).to_rfc3339()],
            )
            .unwrap();
        assert_eq!(pending_albums(&connection, lookups_on()).unwrap().len(), 1);
        assert_eq!(cover::prune_watch(&connection).unwrap(), 1);
        let watched = cover::watched_albums(&connection).unwrap();
        assert_eq!(watched.len(), 1, "the aged entry is gone, the live one stays");
        assert_eq!(watched[0].0, "artist|album");

        // With lookups off, the only work is signing art already held — so an
        // album with nothing resolved must not fill the list.
        assert!(pending_albums(
            &connection,
            CoverPreferences {
                lookup_external: false,
                publish_claims: true,
            },
        )
        .unwrap()
        .is_empty());

        cover::record_art_lookup(
            &connection,
            "artist|album",
            cover::ArtLookupOutcome::Found(&art_lookup("artist|album", "https://archive.org/front.jpg")),
        )
        .unwrap();
        let pending = pending_albums(
            &connection,
            CoverPreferences {
                lookup_external: false,
                publish_claims: true,
            },
        )
        .unwrap();
        assert_eq!(pending.len(), 1, "art already held is the publishable backlog");
        assert_eq!(pending[0].key, "artist|album");
    }

    #[test]
    fn an_http_archive_image_is_upgraded_rather_than_discarded() {
        // The archive answers https for one release group and http for the
        // identical request to another, so a scheme it is inconsistent about
        // must not be read as "nobody has scanned this record".
        assert_eq!(
            secure_image_url("http://coverartarchive.org/release/aa/bb.jpg").as_deref(),
            Some("https://coverartarchive.org/release/aa/bb.jpg")
        );
        assert_eq!(
            secure_image_url("https://coverartarchive.org/release/aa/bb.jpg").as_deref(),
            Some("https://coverartarchive.org/release/aa/bb.jpg")
        );
        // A host we cannot vouch for is still refused rather than guessed at.
        assert_eq!(secure_image_url("http://example.com/a.jpg"), None);
        assert_eq!(secure_image_url("ftp://coverartarchive.org/a.jpg"), None);
        assert_eq!(secure_image_url("  "), None);

        // The real shape of the answer that was being thrown away. The JSON is
        // the archive's own, trimmed to the one image it carried.
        let archive: CoverArtArchive = serde_json::from_str(
            r#"{"images":[{"image":"http://coverartarchive.org/release/4f32f4b8/29167091825.jpg","front":true,"types":["Front"],"thumbnails":{"250":"http://coverartarchive.org/release/4f32f4b8/29167091825-250.jpg","small":"http://coverartarchive.org/release/4f32f4b8/29167091825-250.jpg"}}],"release":"http://musicbrainz.org/release/4f32f4b8"}"#,
        )
        .unwrap();
        assert_eq!(
            front_image(&archive),
            Some((
                "https://coverartarchive.org/release/4f32f4b8/29167091825.jpg".to_string(),
                "https://coverartarchive.org/release/4f32f4b8/29167091825-250.jpg".to_string(),
                true
            )),
            "a front image must survive its scheme being on the wrong side of the archive's inconsistency"
        );

        // A group with no images at all is still a considered "no art".
        let empty: CoverArtArchive = serde_json::from_str(r#"{"images":[]}"#).unwrap();
        assert_eq!(front_image(&empty), None);
    }

    #[test]
    fn a_large_upload_is_published_as_the_archives_own_rendition() {
        // The archive answers with the file that was uploaded, and offers its own
        // smaller renditions beside it. Publishing the upload makes every client
        // carry a scan-sized download for a picture no screen can use all of, so
        // the 1200 rendition is what travels and the upload is the fallback.
        let archive: CoverArtArchive = serde_json::from_str(
            r#"{"images":[{"image":"https://coverartarchive.org/release/aa/bb.jpg","front":true,"thumbnails":{"250":"https://coverartarchive.org/release/aa/bb-250.jpg","500":"https://coverartarchive.org/release/aa/bb-500.jpg","1200":"https://coverartarchive.org/release/aa/bb-1200.jpg","small":"https://coverartarchive.org/release/aa/bb-250.jpg","large":"https://coverartarchive.org/release/aa/bb-500.jpg"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            front_image(&archive),
            Some((
                "https://coverartarchive.org/release/aa/bb-1200.jpg".to_string(),
                "https://coverartarchive.org/release/aa/bb-250.jpg".to_string(),
                true
            )),
            "the 1200 rendition is the picture, and the archive's `large` alias is 500, which is not"
        );

        // An upload smaller than 1200 has no such rendition, and the original is
        // then already the smaller file.
        let smaller_upload: CoverArtArchive = serde_json::from_str(
            r#"{"images":[{"image":"https://coverartarchive.org/release/cc/dd.jpg","front":true,"thumbnails":{"250":"https://coverartarchive.org/release/cc/dd-250.jpg","small":"https://coverartarchive.org/release/cc/dd-250.jpg"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            front_image(&smaller_upload),
            Some((
                "https://coverartarchive.org/release/cc/dd.jpg".to_string(),
                "https://coverartarchive.org/release/cc/dd-250.jpg".to_string(),
                true
            ))
        );

        // A rendition is subject to the same scheme rule as the original.
        let insecure: CoverArtArchive = serde_json::from_str(
            r#"{"images":[{"image":"https://coverartarchive.org/release/ee/ff.jpg","front":true,"thumbnails":{"1200":"http://coverartarchive.org/release/ee/ff-1200.jpg","250":"http://coverartarchive.org/release/ee/ff-250.jpg","small":"http://coverartarchive.org/release/ee/ff-250.jpg"}}]}"#,
        )
        .unwrap();
        assert_eq!(
            front_image(&insecure),
            Some((
                "https://coverartarchive.org/release/ee/ff-1200.jpg".to_string(),
                "https://coverartarchive.org/release/ee/ff-250.jpg".to_string(),
                true
            ))
        );
    }

    #[test]
    fn an_album_release_group_wins_over_a_same_titled_single() {
        let group = |id: &str, title: &str, primary_type: &str| MusicBrainzGroup {
            id: id.to_string(),
            title: title.to_string(),
            first_release_date: "2003-06-05".to_string(),
            primary_type: primary_type.to_string(),
            score: 100,
            secondary_types: Vec::new(),
        };
        // MusicBrainz returns these in score order, and a single's Cover Art
        // Archive entry is usually empty, so taking the first title match would
        // turn a record that has art into "no art anywhere".
        let groups = vec![
            group("single", "St. Anger", "Single"),
            group("ep", "St. Anger", "EP"),
            group("album", "St. Anger", "Album"),
        ];
        assert_eq!(
            best_release_group(&groups, "St. Anger").map(|group| group.id.as_str()),
            Some("album")
        );

        // With nothing but singles, the best match is still better than none.
        let singles = vec![
            group("live", "St. Anger Live Rarities", "Single"),
            group("first", "St. Anger", "Single"),
        ];
        assert_eq!(
            best_release_group(&singles, "St. Anger").map(|group| group.id.as_str()),
            Some("first")
        );

        // A group with no id cannot be resolved, and an unrelated title is not
        // this album however the search ranked it.
        let unusable = vec![
            group("", "St. Anger", "Album"),
            group("other", "Load", "Album"),
        ];
        assert!(best_release_group(&unusable, "St. Anger").is_none());
    }

    #[test]
    fn cover_preferences_survive_a_restart() {
        let path = std::env::temp_dir().join(format!(
            "napstr-cover-preferences-{}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            read_preferences(&path).unwrap(),
            CoverPreferences::default(),
            "with no database at all, both switches are off"
        );
        {
            let connection = crate::open_connection(&path).unwrap();
            connection
                .execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
                .unwrap();
            for (key, enabled) in [(SETTING_LOOKUP_EXTERNAL, true), (SETTING_PUBLISH_CLAIMS, false)]
            {
                connection
                    .execute(
                        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                        params![key, if enabled { "1" } else { "0" }],
                    )
                    .unwrap();
            }
        }
        assert_eq!(
            read_preferences(&path).unwrap(),
            CoverPreferences {
                lookup_external: true,
                publish_claims: false,
            },
            "a choice made in one session must still hold in the next"
        );
        let _ = std::fs::remove_file(&path);
    }
}
