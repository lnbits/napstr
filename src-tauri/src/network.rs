use crate::tor::TorManager;
use crate::transfer::{DownloadOffer, TransferService};
use chrono::Utc;
use futures_util::{stream, StreamExt};
use keyring::Entry;
use nostr_sdk::prelude::Connection as RelayConnection;
use nostr_sdk::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant},
};
use tauri::Emitter;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

pub const CATALOGUE_KIND: u16 = 30421;
pub const AVAILABILITY_KIND: u16 = 30422;
pub const AUDIOBOOK_KIND: u16 = 30423;
const TROLLBOX_HASHTAG: &str = "napstr-trollbox";
const TROLLBOX_MESSAGE_KIND: u16 = 9;
const TRACK_DISCUSSION_PREFIX: &str = "napstr-";
const TRACK_DISCUSSION_SUBSCRIPTION: &str = "napstr-track-discussion";
const PUBLIC_CHAT_EVENT: &str = "napstr-public-chat";
const TRANSFERS_CHANGED_EVENT: &str = "napstr-transfers-changed";
const TROLLBOX_CACHE_LIMIT: usize = 200;
const LIVE_NOSTR_EVENT_LIMIT: usize = 35_000;
const MAX_SEEDER_CANDIDATES: usize = 3;
const CATALOGUE_EVENT_PACE: Duration = Duration::from_millis(75);
const AVAILABILITY_QUERY_LIMIT: usize = 1_000;
const AVAILABILITY_FILE_LIMIT: usize = 50_000;
const AVAILABILITY_CACHE_LIFETIME: Duration = Duration::from_secs(5);
const EMPTY_SEARCH_RESULT_LIMIT: usize = 10_000;
const EMPTY_SEARCH_PAGE_LIMIT: usize = 500;
const CATALOGUE_IDENTIFIER_BATCH_SIZE: usize = 75;
const CATALOGUE_IDENTIFIER_CONCURRENCY: usize = 8;
const CATALOGUE_BROWSE_SESSION_LIFETIME: Duration = Duration::from_secs(10 * 60);
const CATALOGUE_BROWSE_SESSION_LIMIT: usize = 8;
const NETWORK_SEARCH_RESULT_LIMIT: usize = 500;
const CATALOGUE_CACHE_SCAN_LIMIT: usize = 25_000;
const CATALOGUE_SEARCH_TOKEN_LIMIT: usize = 20;
const CATALOGUE_SEARCH_TOKEN_LENGTH: usize = 32;
const CATALOGUE_QUERY_TOKEN_LIMIT: usize = 4;
const AUDIOBOOK_RESULT_LIMIT: usize = 250;
const AUDIOBOOK_CHAPTER_LIMIT: usize = 500;
const AUDIOBOOK_MANIFEST_BYTE_LIMIT: usize = 128 * 1024;
const CATALOGUE_SEARCH_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "audio", "be", "by", "flac", "for", "from", "in", "is",
    "it", "mp3", "of", "ogg", "on", "opus", "or", "the", "to", "wav", "with",
];

pub fn validate_profile_picture(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Ok(());
    }
    let url = Url::parse(value).map_err(|error| format!("invalid profile picture URL: {error}"))?;
    if url.scheme() != "https" {
        return Err("profile picture URL must use HTTPS".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    pub connected: bool,
    pub npub: String,
    pub pubkey: String,
    pub relay_count: usize,
    pub relays_via_tor: bool,
    pub tor_running: bool,
    pub tor_starting: bool,
    pub tor_progress: u8,
    pub tor_error: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueSource {
    pub pubkey: String,
    pub npub: String,
    pub display_name: String,
    pub relay: String,
    pub about: String,
    pub picture: String,
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueResult {
    pub file_id: String,
    pub filename: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub format: String,
    pub mime: String,
    pub size: u64,
    pub license: String,
    pub description: String,
    pub tags: String,
    pub sources: Vec<CatalogueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookChapter {
    pub position: usize,
    pub file_id: String,
    pub filename: String,
    pub title: String,
    pub format: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookResult {
    pub audiobook_id: String,
    pub title: String,
    pub author: String,
    pub narrator: String,
    pub total_size: u64,
    pub chapters: Vec<AudiobookChapter>,
    pub sources: Vec<CatalogueSource>,
    pub local: bool,
    pub local_folder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AudiobookContent {
    protocol: String,
    audiobook_id: String,
    title: String,
    author: String,
    narrator: String,
    total_size: u64,
    chapters: Vec<AudiobookChapter>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueBrowseCursor {
    session_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueBrowsePage {
    pub results: Vec<CatalogueResult>,
    pub cursor: Option<CatalogueBrowseCursor>,
    pub total_available: usize,
}

struct CatalogueBrowseSession {
    created_at: Instant,
    online: HashSet<(String, String)>,
    available_by_file: HashMap<String, HashSet<String>>,
    pending_file_ids: VecDeque<String>,
    total_available: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrollboxMessage {
    pub event_id: String,
    pub pubkey: String,
    pub npub: String,
    pub display_name: String,
    pub content: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogueContent {
    protocol: String,
    file_id: String,
    filename: String,
    title: String,
    artist: String,
    album: String,
    format: String,
    mime: String,
    size: u64,
    license: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: String,
}

#[derive(Debug)]
struct CachedCatalogueRecord {
    file_id: String,
    source_pubkey: String,
    filename: String,
    title: String,
    artist: String,
    album: String,
    format: String,
    mime: String,
    size: u64,
    tags: String,
    event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum SignalMessage {
    DownloadRequest {
        protocol: String,
        request_id: String,
        file_id: String,
    },
    DownloadOffer {
        protocol: String,
        offer: DownloadOffer,
    },
    DownloadRefused {
        protocol: String,
        request_id: String,
        file_id: String,
        reason: String,
    },
}

fn availability_search_filter() -> Filter {
    Filter::new()
        .kind(Kind::from(AVAILABILITY_KIND))
        .hashtag("napstr-availability")
        .since(Timestamp::from(
            Utc::now().timestamp().saturating_sub(12 * 60) as u64,
        ))
        .limit(AVAILABILITY_QUERY_LIMIT)
}

fn catalogue_name_search_filter(query: &str) -> Filter {
    Filter::new()
        .kind(Kind::from(CATALOGUE_KIND))
        .hashtag("napstr")
        .search(query)
        .limit(NETWORK_SEARCH_RESULT_LIMIT)
}

fn catalogue_identifier_filter(file_ids: &[String]) -> Filter {
    Filter::new()
        .kind(Kind::from(CATALOGUE_KIND))
        .hashtag("napstr")
        .identifiers(file_ids.iter().cloned())
        .limit(file_ids.len().saturating_mul(8).clamp(1, 1_000))
}

async fn fetch_catalogue_identifiers(
    client: &Client,
    file_ids: &[String],
) -> Result<(Vec<Event>, Vec<String>), String> {
    let batches = file_ids
        .chunks(CATALOGUE_IDENTIFIER_BATCH_SIZE)
        .map(|batch| batch.to_vec())
        .collect::<Vec<_>>();
    let fetched = stream::iter(batches)
        .map(|batch| {
            let client = client.clone();
            async move {
                let result = client
                    .fetch_events(catalogue_identifier_filter(&batch), Duration::from_secs(8))
                    .await;
                (batch, result)
            }
        })
        .buffer_unordered(CATALOGUE_IDENTIFIER_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut successful_batches = 0usize;
    let mut failures = Vec::new();
    let mut retry_file_ids = Vec::new();
    let mut events_by_id = HashMap::new();
    for (batch, result) in fetched {
        match result {
            Ok(events) => {
                successful_batches += 1;
                for event in events.iter() {
                    events_by_id.insert(event.id, event.clone());
                }
            }
            Err(error) => {
                failures.push(error.to_string());
                retry_file_ids.extend(batch);
            }
        }
    }
    if successful_batches == 0 && !file_ids.is_empty() {
        return Err(format!(
            "catalogue identifier lookup failed: {}",
            failures
                .first()
                .map(String::as_str)
                .unwrap_or("all relay queries failed")
        ));
    }
    Ok((events_by_id.into_values().collect(), retry_file_ids))
}

fn catalogue_search_tokens(fields: &[&str]) -> Vec<String> {
    let mut seen = HashSet::new();
    let field_tokens = fields
        .iter()
        .map(|field| {
            super::search_tokens(field)
                .into_iter()
                .filter(|token| {
                    token != "napstr" && !CATALOGUE_SEARCH_STOP_WORDS.contains(&token.as_str())
                })
                .filter(|token| token.chars().count() <= CATALOGUE_SEARCH_TOKEN_LENGTH)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut tokens = Vec::new();
    for index in 0..field_tokens.iter().map(Vec::len).max().unwrap_or(0) {
        for field in &field_tokens {
            if let Some(token) = field.get(index) {
                if seen.insert(token.clone()) {
                    tokens.push(token.clone());
                    if tokens.len() == CATALOGUE_SEARCH_TOKEN_LIMIT {
                        return tokens;
                    }
                }
            }
        }
    }
    tokens
}

fn catalogue_tag_search_filters(query: &str) -> Vec<Filter> {
    let mut tokens = catalogue_search_tokens(&[query]);
    tokens.sort_by(|left, right| {
        right
            .chars()
            .count()
            .cmp(&left.chars().count())
            .then_with(|| left.cmp(right))
    });
    tokens
        .into_iter()
        .take(CATALOGUE_QUERY_TOKEN_LIMIT)
        .map(|token| {
            Filter::new()
                .kind(Kind::from(CATALOGUE_KIND))
                .custom_tag(SingleLetterTag::lowercase(Alphabet::T), token)
                .limit(NETWORK_SEARCH_RESULT_LIMIT)
        })
        .collect()
}

fn catalogue_event_fingerprint(content_json: &str, search_tokens: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"napstr-catalogue-event-v1\0");
    digest.update(content_json.as_bytes());
    for token in search_tokens {
        digest.update((token.len() as u64).to_be_bytes());
        digest.update(token.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn audiobook_event_fingerprint(content_json: &str, search_tokens: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"napstr-audiobook-event-v1\0");
    digest.update(content_json.as_bytes());
    for token in search_tokens {
        digest.update((token.len() as u64).to_be_bytes());
        digest.update(token.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn audiobook_id(chapters: &[AudiobookChapter]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"napstr-audiobook-v1\0");
    for chapter in chapters {
        digest.update(chapter.file_id.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn valid_audiobook_event(event: &Event, content: &AudiobookContent) -> bool {
    if event.kind != Kind::from(AUDIOBOOK_KIND)
        || event.verify().is_err()
        || event.tags.identifier() != Some(content.audiobook_id.as_str())
        || !event.tags.hashtags().any(|tag| tag == "napstr-audiobook")
        || content.protocol != "napstr/1"
        || event.content.len() > AUDIOBOOK_MANIFEST_BYTE_LIMIT
        || content.chapters.is_empty()
        || content.chapters.len() > AUDIOBOOK_CHAPTER_LIMIT
        || content.audiobook_id != audiobook_id(&content.chapters)
        || !valid_catalogue_metadata(&[&content.title, &content.author, &content.narrator])
        || content.title.is_empty()
    {
        return false;
    }
    let mut ids = HashSet::new();
    let mut total_size = 0u64;
    for (index, chapter) in content.chapters.iter().enumerate() {
        if chapter.position != index + 1
            || !valid_file_id(&chapter.file_id)
            || chapter.size == 0
            || !ids.insert(chapter.file_id.as_str())
            || chapter.filename.contains('/')
            || chapter.filename.contains('\\')
            || !audio_claim_valid(&chapter.filename, &chapter.format, &chapter.mime)
            || PathBuf::from(&chapter.filename)
                .file_name()
                .and_then(|value| value.to_str())
                != Some(chapter.filename.as_str())
            || !valid_catalogue_metadata(&[&chapter.filename, &chapter.title])
        {
            return false;
        }
        let Some(next_total) = total_size.checked_add(chapter.size) else {
            return false;
        };
        total_size = next_total;
    }
    total_size == content.total_size
}

fn catalogue_publication_is_current(existing: Option<&String>, fingerprint: &str) -> bool {
    existing.is_some_and(|existing| !existing.is_empty() && existing == fingerprint)
}

fn valid_file_id(file_id: &str) -> bool {
    hex::decode(file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
}

fn valid_catalogue_metadata(values: &[&str]) -> bool {
    values.iter().all(|value| {
        value.chars().count() <= 256 && !value.chars().any(is_unsafe_public_chat_character)
    })
}

fn valid_catalogue_event(event: &Event, content: &CatalogueContent) -> bool {
    event.kind == Kind::from(CATALOGUE_KIND)
        && event.verify().is_ok()
        && event.tags.identifier() == Some(content.file_id.as_str())
        && event.tags.hashtags().any(|tag| tag == "napstr")
}

fn merge_availability_events<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    online: &mut HashSet<(String, String)>,
    available_by_file: &mut HashMap<String, HashSet<String>>,
) {
    for event in events {
        if event.kind != Kind::from(AVAILABILITY_KIND)
            || event.verify().is_err()
            || !event
                .tags
                .hashtags()
                .any(|tag| tag == "napstr-availability")
            || event
                .tags
                .expiration()
                .map(|expires| *expires <= Timestamp::now())
                .unwrap_or(true)
        {
            continue;
        }
        let Ok(ids) = serde_json::from_str::<Vec<String>>(&event.content) else {
            continue;
        };
        if ids.len() > 400 {
            continue;
        }
        let pubkey = event.pubkey.to_hex();
        for id in ids {
            if !valid_file_id(&id) {
                continue;
            }
            if online.len() >= AVAILABILITY_FILE_LIMIT {
                return;
            }
            online.insert((pubkey.clone(), id.clone()));
            available_by_file
                .entry(id)
                .or_default()
                .insert(pubkey.clone());
        }
    }
}

fn merge_catalogue_result(
    aggregated: &mut HashMap<String, CatalogueResult>,
    content: CatalogueContent,
    catalogue_tags: String,
    source: CatalogueSource,
) {
    let catalogue_name = content.filename.clone();
    let catalogue_title = if content.title.is_empty() {
        catalogue_name.clone()
    } else {
        content.title.clone()
    };
    aggregated
        .entry(content.file_id.clone())
        .and_modify(|item| {
            if !item
                .sources
                .iter()
                .any(|existing| existing.pubkey == source.pubkey)
            {
                item.sources.push(source.clone());
            }
            if item.tags.is_empty() {
                item.tags = catalogue_tags.clone();
            }
        })
        .or_insert(CatalogueResult {
            file_id: content.file_id,
            filename: catalogue_name,
            title: catalogue_title,
            artist: content.artist,
            album: content.album,
            format: content.format,
            mime: content.mime,
            size: content.size,
            license: "unspecified".into(),
            description: String::new(),
            tags: catalogue_tags,
            sources: vec![source],
        });
}

struct AvailabilitySnapshot {
    fetched_at: Instant,
    online: HashSet<(String, String)>,
    available_by_file: HashMap<String, HashSet<String>>,
}

pub struct NetworkService {
    db_path: PathBuf,
    transfers: Arc<TransferService>,
    tor: Arc<TorManager>,
    app_handle: tauri::AppHandle,
    client: RwLock<Option<Client>>,
    keys: RwLock<Option<Keys>>,
    start_lock: Mutex<()>,
    catalogue_publish_lock: Mutex<()>,
    catalogue_publish_requested: AtomicBool,
    catalogue_availability_requested: AtomicBool,
    catalogue_reconcile_requested: AtomicBool,
    catalogue_pending_ids: StdMutex<HashSet<String>>,
    catalogue_publish_worker_running: AtomicBool,
    catalogue_browse_sessions: Mutex<HashMap<String, CatalogueBrowseSession>>,
    availability_cache: RwLock<Option<Arc<AvailabilitySnapshot>>>,
    availability_fetch_lock: Mutex<()>,
    trollbox_cache_lock: Mutex<()>,
    track_discussion_subscription_lock: Mutex<()>,
    connected: AtomicBool,
    relays_via_tor: AtomicBool,
    generation: AtomicU64,
    last_error: RwLock<String>,
    trollbox_profiles: RwLock<HashMap<String, String>>,
}

impl NetworkService {
    pub fn new(
        db_path: PathBuf,
        transfers: Arc<TransferService>,
        tor: Arc<TorManager>,
        app_handle: tauri::AppHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            db_path,
            transfers,
            tor,
            app_handle,
            client: RwLock::new(None),
            keys: RwLock::new(None),
            start_lock: Mutex::new(()),
            catalogue_publish_lock: Mutex::new(()),
            catalogue_publish_requested: AtomicBool::new(false),
            catalogue_availability_requested: AtomicBool::new(false),
            catalogue_reconcile_requested: AtomicBool::new(false),
            catalogue_pending_ids: StdMutex::new(HashSet::new()),
            catalogue_publish_worker_running: AtomicBool::new(false),
            catalogue_browse_sessions: Mutex::new(HashMap::new()),
            availability_cache: RwLock::new(None),
            availability_fetch_lock: Mutex::new(()),
            trollbox_cache_lock: Mutex::new(()),
            track_discussion_subscription_lock: Mutex::new(()),
            connected: AtomicBool::new(false),
            relays_via_tor: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            last_error: RwLock::new(String::new()),
            trollbox_profiles: RwLock::new(HashMap::new()),
        })
    }

    pub fn transfers(&self) -> &Arc<TransferService> {
        &self.transfers
    }

    async fn availability_snapshot(
        &self,
        client: &Client,
    ) -> Result<Arc<AvailabilitySnapshot>, String> {
        if let Some(snapshot) = self.availability_cache.read().await.as_ref() {
            if snapshot.fetched_at.elapsed() < AVAILABILITY_CACHE_LIFETIME {
                return Ok(snapshot.clone());
            }
        }
        // Coalesce track and audiobook searches started by the same UI action
        // into one relay heartbeat query.
        let _fetch_guard = self.availability_fetch_lock.lock().await;
        if let Some(snapshot) = self.availability_cache.read().await.as_ref() {
            if snapshot.fetched_at.elapsed() < AVAILABILITY_CACHE_LIFETIME {
                return Ok(snapshot.clone());
            }
        }
        let events = client
            .fetch_events(availability_search_filter(), Duration::from_secs(6))
            .await
            .map_err(|error| format!("availability search failed: {error}"))?;
        let mut snapshot = AvailabilitySnapshot {
            fetched_at: Instant::now(),
            online: HashSet::new(),
            available_by_file: HashMap::new(),
        };
        merge_availability_events(
            events.iter(),
            &mut snapshot.online,
            &mut snapshot.available_by_file,
        );
        let snapshot = Arc::new(snapshot);
        *self.availability_cache.write().await = Some(snapshot.clone());
        Ok(snapshot)
    }

    pub fn queue_catalogue_publish(self: &Arc<Self>, force_availability: bool) {
        self.catalogue_reconcile_requested
            .store(true, Ordering::SeqCst);
        self.queue_catalogue_work(std::iter::empty(), force_availability);
    }

    pub fn queue_catalogue_files(self: &Arc<Self>, file_ids: impl IntoIterator<Item = String>) {
        self.queue_catalogue_work(file_ids, false);
    }

    fn queue_catalogue_work(
        self: &Arc<Self>,
        file_ids: impl IntoIterator<Item = String>,
        force_availability: bool,
    ) {
        if let Ok(mut pending) = self.catalogue_pending_ids.lock() {
            pending.extend(file_ids);
        } else {
            self.catalogue_reconcile_requested
                .store(true, Ordering::SeqCst);
        }
        self.catalogue_publish_requested
            .store(true, Ordering::SeqCst);
        if force_availability {
            self.catalogue_availability_requested
                .store(true, Ordering::SeqCst);
        }
        if !self.connected.load(Ordering::SeqCst)
            || self
                .catalogue_publish_worker_running
                .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut retry_delay = 1u64;
            while service.connected.load(Ordering::SeqCst)
                && service
                    .catalogue_publish_requested
                    .swap(false, Ordering::SeqCst)
            {
                let force_availability = service
                    .catalogue_availability_requested
                    .swap(false, Ordering::SeqCst);
                let reconcile = service
                    .catalogue_reconcile_requested
                    .swap(false, Ordering::SeqCst);
                let pending_ids = service
                    .catalogue_pending_ids
                    .lock()
                    .map(|mut pending| pending.drain().collect::<HashSet<_>>())
                    .unwrap_or_default();
                match service
                    .publish_catalogue_once(&pending_ids, reconcile, force_availability)
                    .await
                {
                    Ok(_) => {
                        retry_delay = 1;
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                    Err(_) => {
                        if let Ok(mut pending) = service.catalogue_pending_ids.lock() {
                            pending.extend(pending_ids);
                        }
                        if reconcile {
                            service
                                .catalogue_reconcile_requested
                                .store(true, Ordering::SeqCst);
                        }
                        service
                            .catalogue_publish_requested
                            .store(true, Ordering::SeqCst);
                        service
                            .catalogue_availability_requested
                            .store(true, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(retry_delay)).await;
                        retry_delay = (retry_delay * 2).min(30);
                    }
                }
            }
            service
                .catalogue_publish_worker_running
                .store(false, Ordering::SeqCst);
            if service.connected.load(Ordering::SeqCst)
                && service.catalogue_publish_requested.load(Ordering::SeqCst)
            {
                service.queue_catalogue_work(std::iter::empty(), false);
            }
        });
    }

    pub async fn start(self: &Arc<Self>) -> Result<NetworkStatus, String> {
        let _start_guard = self.start_lock.lock().await;
        if self.connected.load(Ordering::SeqCst) {
            return self.status().await;
        }
        let keys = load_or_create_identity()?;
        let connection = super::open_connection(&self.db_path)?;
        let relays = relay_urls(&super::get_setting(&connection, "nostr_relays")?);
        if relays.is_empty() {
            return Err("at least one Nostr relay is required".into());
        }
        let relays_over_tor = super::get_setting(&connection, "relays_over_tor")
            .map(|value| value == "on")
            .unwrap_or(false);
        drop(connection);

        let proxy = if relays_over_tor {
            let socks_port = self.tor.start().await.map_err(|error| {
                format!("private connection mode needs the built-in Tor: {error}")
            })?;
            Some(SocketAddr::from(([127, 0, 0, 1], socks_port)))
        } else {
            None
        };
        let client = nostr_client(keys.clone(), proxy);
        // Chat history is an optional local acceleration layer and must never
        // prevent the Nostr client itself from connecting.
        let _ = self.hydrate_trollbox_cache(&client).await;
        client.automatic_authentication(true);
        for relay in &relays {
            client
                .add_relay(relay)
                .await
                .map_err(|error| format!("relay {relay}: {error}"))?;
        }
        client.connect().await;

        self.publish_profile_with_client(&client, keys.public_key())
            .await?;
        let dm_tags = relays
            .iter()
            .map(|relay| Tag::parse(["relay", relay.as_str()]).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        client
            .send_event_builder(EventBuilder::new(Kind::from(10050), "").tags(dm_tags))
            .await
            .map_err(|error| format!("DM relay publication failed: {error}"))?;

        let inbox_filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(keys.public_key())
            .limit(0);
        client
            .subscribe(inbox_filter, None)
            .await
            .map_err(|error| format!("NIP-17 inbox subscription failed: {error}"))?;
        client
            .subscribe(trollbox_filter(TROLLBOX_CACHE_LIMIT), None)
            .await
            .map_err(|error| format!("public trollbox subscription failed: {error}"))?;
        *self.client.write().await = Some(client.clone());
        *self.keys.write().await = Some(keys);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.relays_via_tor.store(relays_over_tor, Ordering::SeqCst);
        self.connected.store(true, Ordering::SeqCst);
        *self.last_error.write().await = String::new();

        let service = self.clone();
        tokio::spawn(async move {
            let listener_client = client.clone();
            let event_client = listener_client.clone();
            let event_service = service.clone();
            let result = listener_client
                .handle_notifications(move |notification| {
                    let service = event_service.clone();
                    let client = event_client.clone();
                    async move {
                        if let RelayPoolNotification::Event { event, .. } = notification {
                            if event.kind == Kind::GiftWrap {
                                if let Ok(unwrapped) = client.unwrap_gift_wrap(&event).await {
                                    let unexpired = unwrapped
                                        .rumor
                                        .tags
                                        .expiration()
                                        .map(|expires| *expires > Timestamp::now())
                                        .unwrap_or(false);
                                    if unwrapped.rumor.kind == Kind::PrivateDirectMessage
                                        && unexpired
                                    {
                                        let _ = service
                                            .handle_signal(
                                                unwrapped.sender,
                                                &unwrapped.rumor.content,
                                            )
                                            .await;
                                    }
                                }
                            } else if let Some(topic) = public_chat_topic(&event) {
                                if topic == TROLLBOX_HASHTAG {
                                    let cache_service = service.clone();
                                    let cache_event = (*event).clone();
                                    tokio::spawn(async move {
                                        let _ =
                                            cache_service.cache_trollbox_event(cache_event).await;
                                    });
                                }
                                let _ = service.app_handle.emit(PUBLIC_CHAT_EVENT, topic);
                            }
                        }
                        Ok(false)
                    }
                })
                .await;
            if let Err(error) = result {
                if service.generation.load(Ordering::SeqCst) == generation {
                    service.connected.store(false, Ordering::SeqCst);
                    *service.last_error.write().await = error.to_string();
                }
            }
        });

        self.queue_catalogue_publish(true);
        let heartbeat = self.clone();
        tokio::spawn(async move {
            while heartbeat.connected.load(Ordering::SeqCst)
                && heartbeat.generation.load(Ordering::SeqCst) == generation
            {
                tokio::time::sleep(Duration::from_secs(240)).await;
                if heartbeat.connected.load(Ordering::SeqCst)
                    && heartbeat.generation.load(Ordering::SeqCst) == generation
                {
                    let _ = heartbeat.publish_availability().await;
                }
            }
        });
        self.status().await
    }

    pub async fn stop(&self) {
        self.connected.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(client) = self.client.write().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), client.disconnect()).await;
        }
    }

    pub async fn clear_cached_identity(&self) {
        *self.keys.write().await = None;
    }

    pub async fn restart(self: &Arc<Self>) -> Result<NetworkStatus, String> {
        self.stop().await;
        match self.start().await {
            Ok(status) => Ok(status),
            Err(error) => {
                *self.last_error.write().await = error.clone();
                Err(error)
            }
        }
    }

    pub async fn status(&self) -> Result<NetworkStatus, String> {
        let keys = self
            .keys
            .read()
            .await
            .clone()
            .or_else(|| load_or_create_identity().ok());
        let (npub, pubkey) = match keys {
            Some(keys) => (
                keys.public_key()
                    .to_bech32()
                    .map_err(|error| error.to_string())?,
                keys.public_key().to_hex(),
            ),
            None => (String::new(), String::new()),
        };
        let relay_count = super::open_connection(&self.db_path)
            .ok()
            .and_then(|connection| super::get_setting(&connection, "nostr_relays").ok())
            .map(|value| relay_urls(&value).len())
            .unwrap_or(0);
        Ok(NetworkStatus {
            connected: self.connected.load(Ordering::SeqCst),
            npub,
            pubkey,
            relay_count,
            relays_via_tor: self.connected.load(Ordering::SeqCst)
                && self.relays_via_tor.load(Ordering::SeqCst),
            tor_running: false,
            tor_starting: false,
            tor_progress: 0,
            tor_error: String::new(),
            error: self.last_error.read().await.clone(),
        })
    }

    async fn publish_catalogue_once(
        &self,
        pending_ids: &HashSet<String>,
        reconcile: bool,
        force_availability: bool,
    ) -> Result<usize, String> {
        let _publish_guard = self.catalogue_publish_lock.lock().await;
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let db_path = self.db_path.clone();
        let pending_ids = pending_ids.clone();
        let (files, published_fingerprints, audiobooks, published_audiobooks) =
            tauri::async_runtime::spawn_blocking(move || {
                let files = if reconcile {
                    load_publish_files(&db_path)?
                } else {
                    load_publish_files_by_id(&db_path, &pending_ids)?
                };
                let published_fingerprints = if reconcile {
                    load_published_fingerprints(&db_path)?
                } else {
                    load_published_fingerprints_by_id(&db_path, &pending_ids)?
                };
                // File events can be published progressively while a scan is
                // running. Collection manifests describe the complete ordered
                // set, so rebuilding them for every 50-file batch only creates
                // transient editions and relay churn. The scan's final
                // reconciliation (and explicit audiobook edits) handles them.
                let (audiobooks, published_audiobooks) = if reconcile {
                    let connection = super::open_connection(&db_path)?;
                    (
                        super::build_local_audiobooks(&connection)?,
                        load_published_audiobooks(&connection)?,
                    )
                } else {
                    (Vec::new(), HashMap::new())
                };
                Ok::<_, String>((
                    files,
                    published_fingerprints,
                    audiobooks,
                    published_audiobooks,
                ))
            })
            .await
            .map_err(|error| format!("catalogue preparation task failed: {error}"))??;
        if !files.is_empty() {
            let transfers = self.transfers.clone();
            tokio::spawn(async move {
                let _ = transfers.warm_for_sharing().await;
            });
        }
        let publication_db = super::open_connection(&self.db_path)?;
        let current_ids: HashSet<String> = files.iter().map(|file| file.0.clone()).collect();
        let stale = if reconcile {
            published_fingerprints
                .keys()
                .filter(|id| !current_ids.contains(*id))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for file_id in stale {
            let tags = vec![
                Tag::parse(["d", file_id.as_str()]),
                Tag::parse(["t", "napstr"]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            client
                .send_event_builder(
                    EventBuilder::new(
                        Kind::from(CATALOGUE_KIND),
                        r#"{"protocol":"napstr/1","deleted":true}"#,
                    )
                    .tags(tags),
                )
                .await
                .map_err(|error| format!("catalogue withdrawal failed: {error}"))?;
            publication_db
                .execute(
                    "DELETE FROM published_catalogue WHERE file_id=?1",
                    [&file_id],
                )
                .map_err(|error| error.to_string())?;
            tokio::time::sleep(CATALOGUE_EVENT_PACE).await;
        }
        let mut published = 0;
        let mut published_ids = Vec::new();
        for (file_id, filename, size, format, mime, catalogue_tags, title, artist, album) in files {
            let search_tokens =
                catalogue_search_tokens(&[&filename, &title, &artist, &album, &catalogue_tags]);
            let content = CatalogueContent {
                protocol: "napstr/1".into(),
                file_id: file_id.clone(),
                filename: filename.clone(),
                title,
                artist,
                album,
                format: format.clone(),
                mime: if mime.is_empty() || mime == "application/octet-stream" {
                    mime_for_format(&format)
                } else {
                    mime
                },
                size,
                license: "unspecified".into(),
                description: String::new(),
                tags: catalogue_tags,
            };
            let content_json =
                serde_json::to_string(&content).map_err(|error| error.to_string())?;
            let fingerprint = catalogue_event_fingerprint(&content_json, &search_tokens);
            if catalogue_publication_is_current(published_fingerprints.get(&file_id), &fingerprint)
            {
                continue;
            }
            let mut tags = vec![
                Tag::parse(["d", file_id.as_str()]),
                Tag::parse(["t", "napstr"]),
                Tag::parse(["x", file_id.as_str()]),
                Tag::parse(["name", filename.as_str()]),
                Tag::parse(["size", &size.to_string()]),
                Tag::parse(["m", content.mime.as_str()]),
                Tag::parse(["alt", "Napstr shared file catalogue entry"]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            for token in search_tokens {
                tags.push(Tag::parse(["t", token.as_str()]).map_err(|error| error.to_string())?);
            }
            client
                .send_event_builder(
                    EventBuilder::new(Kind::from(CATALOGUE_KIND), content_json).tags(tags),
                )
                .await
                .map_err(|error| format!("catalogue publication failed for {filename}: {error}"))?;
            publication_db.execute("INSERT OR REPLACE INTO published_catalogue(file_id,published_at,fingerprint) VALUES (?1,?2,?3)", params![file_id, Utc::now().to_rfc3339(), fingerprint]).map_err(|error| error.to_string())?;
            published += 1;
            published_ids.push(file_id);
            tokio::time::sleep(CATALOGUE_EVENT_PACE).await;
        }
        let current_folders = audiobooks
            .iter()
            .map(|book| book.local_folder.clone())
            .collect::<HashSet<_>>();
        for (folder, (audiobook_id, _)) in published_audiobooks
            .iter()
            .filter(|(folder, _)| !current_folders.contains(*folder))
        {
            let tags = vec![
                Tag::parse(["d", audiobook_id.as_str()]),
                Tag::parse(["t", "napstr-audiobook"]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            client
                .send_event_builder(
                    EventBuilder::new(
                        Kind::from(AUDIOBOOK_KIND),
                        r#"{"protocol":"napstr/1","deleted":true}"#,
                    )
                    .tags(tags),
                )
                .await
                .map_err(|error| format!("audiobook withdrawal failed: {error}"))?;
            publication_db
                .execute("DELETE FROM published_audiobooks WHERE folder=?1", [folder])
                .map_err(|error| error.to_string())?;
        }
        for book in audiobooks {
            if let Some((old_id, _)) = published_audiobooks.get(&book.local_folder) {
                if old_id != &book.audiobook_id {
                    let tags = vec![
                        Tag::parse(["d", old_id.as_str()]),
                        Tag::parse(["t", "napstr-audiobook"]),
                    ]
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?;
                    client
                        .send_event_builder(
                            EventBuilder::new(
                                Kind::from(AUDIOBOOK_KIND),
                                r#"{"protocol":"napstr/1","deleted":true}"#,
                            )
                            .tags(tags),
                        )
                        .await
                        .map_err(|error| {
                            format!("old audiobook edition withdrawal failed: {error}")
                        })?;
                }
            }
            let content = AudiobookContent {
                protocol: "napstr/1".into(),
                audiobook_id: book.audiobook_id.clone(),
                title: book.title.clone(),
                author: book.author.clone(),
                narrator: book.narrator.clone(),
                total_size: book.total_size,
                chapters: book.chapters.clone(),
            };
            let content_json =
                serde_json::to_string(&content).map_err(|error| error.to_string())?;
            let mut search_values = vec![
                book.title.as_str(),
                book.author.as_str(),
                book.narrator.as_str(),
            ];
            search_values.extend(
                book.chapters
                    .iter()
                    .flat_map(|chapter| [chapter.title.as_str(), chapter.filename.as_str()]),
            );
            let search_tokens = catalogue_search_tokens(&search_values);
            let fingerprint = audiobook_event_fingerprint(&content_json, &search_tokens);
            if published_audiobooks
                .get(&book.local_folder)
                .is_some_and(|(id, existing)| id == &book.audiobook_id && existing == &fingerprint)
            {
                continue;
            }
            let mut tags = vec![
                Tag::parse(["d", book.audiobook_id.as_str()]),
                Tag::parse(["t", "napstr-audiobook"]),
                Tag::parse(["x", book.audiobook_id.as_str()]),
                Tag::parse(["title", book.title.as_str()]),
                Tag::parse(["alt", "Napstr audiobook manifest"]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            for token in search_tokens {
                tags.push(Tag::parse(["t", token.as_str()]).map_err(|error| error.to_string())?);
            }
            client
                .send_event_builder(
                    EventBuilder::new(Kind::from(AUDIOBOOK_KIND), content_json).tags(tags),
                )
                .await
                .map_err(|error| {
                    format!("audiobook publication failed for {}: {error}", book.title)
                })?;
            publication_db.execute(
                "INSERT OR REPLACE INTO published_audiobooks(folder,audiobook_id,fingerprint,published_at) VALUES(?1,?2,?3,?4)",
                params![book.local_folder, book.audiobook_id, fingerprint, Utc::now().to_rfc3339()],
            ).map_err(|error| error.to_string())?;
            tokio::time::sleep(CATALOGUE_EVENT_PACE).await;
        }
        if force_availability {
            self.publish_availability().await?;
        } else if !published_ids.is_empty() {
            self.publish_availability_delta(&client, &published_ids)
                .await?;
        }
        Ok(published)
    }

    pub async fn publish_profile(&self) -> Result<(), String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let public_key = self
            .keys
            .read()
            .await
            .as_ref()
            .map(Keys::public_key)
            .ok_or("Nostr identity is not loaded")?;
        self.publish_profile_with_client(&client, public_key).await
    }

    pub async fn trollbox_messages(&self) -> Result<Vec<TrollboxMessage>, String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        self.public_chat_messages(
            &client,
            trollbox_filter(TROLLBOX_CACHE_LIMIT),
            TROLLBOX_HASHTAG,
        )
        .await
    }

    pub async fn track_discussion_messages(
        &self,
        file_id: String,
        subscribe: bool,
    ) -> Result<Vec<TrollboxMessage>, String> {
        let topic = track_discussion_topic(&file_id)?;
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        if subscribe {
            // Track selection can change while a previous IPC request is still
            // awaiting relays. Serialize replacement so the newest selection
            // cannot be overwritten by an older in-flight subscription.
            let _subscription_guard = self.track_discussion_subscription_lock.lock().await;
            let subscription_id = SubscriptionId::new(TRACK_DISCUSSION_SUBSCRIPTION);
            client.unsubscribe(&subscription_id).await;
            client
                .subscribe_with_id(subscription_id, public_chat_filter(&topic, 100), None)
                .await
                .map_err(|error| format!("track discussion subscription failed: {error}"))?;
        }
        self.public_chat_messages(&client, public_chat_filter(&topic, 100), &topic)
            .await
    }

    async fn public_chat_messages(
        &self,
        client: &Client,
        filter: Filter,
        topic: &str,
    ) -> Result<Vec<TrollboxMessage>, String> {
        let events = client
            .database()
            .query(filter)
            .await
            .map_err(|error| format!("could not read the public chat cache: {error}"))?;
        let blocked = blocked_pubkeys(&self.db_path)?;
        let mut chat_events = events
            .iter()
            .filter(|event| {
                event.kind == Kind::from(TROLLBOX_MESSAGE_KIND)
                    && event
                        .tags
                        .iter()
                        .any(|tag| tag.kind() == TagKind::t() && tag.content() == Some(topic))
                    && !blocked.contains(&event.pubkey.to_hex())
                    && !event.content.trim().is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        chat_events.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        let current_key = self
            .keys
            .read()
            .await
            .as_ref()
            .map(Keys::public_key)
            .ok_or("Nostr identity is not loaded")?;
        let current_name =
            super::get_setting(&super::open_connection(&self.db_path)?, "display_name")?;
        self.trollbox_profiles
            .write()
            .await
            .insert(current_key.to_hex(), safe_trollbox_name(&current_name));

        let cached = self.trollbox_profiles.read().await.clone();
        let missing = chat_events
            .iter()
            .map(|event| event.pubkey)
            .filter(|pubkey| !cached.contains_key(&pubkey.to_hex()))
            .collect::<HashSet<_>>()
            .into_iter()
            .take(64)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let metadata_events = client
                .fetch_events(
                    Filter::new()
                        .kind(Kind::Metadata)
                        .authors(missing.iter().copied())
                        .limit(missing.len()),
                    Duration::from_secs(3),
                )
                .await
                .ok();
            let mut discovered = HashMap::new();
            if let Some(events) = metadata_events {
                for event in events.iter() {
                    if let Ok(metadata) = Metadata::from_json(&event.content) {
                        if let Some(name) = metadata.display_name.or(metadata.name) {
                            discovered.insert(event.pubkey.to_hex(), safe_trollbox_name(&name));
                        }
                    }
                }
            }
            let profiles = missing
                .into_iter()
                .map(|pubkey| {
                    let pubkey = pubkey.to_hex();
                    let name = discovered
                        .remove(&pubkey)
                        .unwrap_or_else(|| "napstr-user".into());
                    (pubkey, name)
                })
                .collect::<Vec<_>>();
            self.trollbox_profiles.write().await.extend(profiles);
        }
        let profiles = self.trollbox_profiles.read().await;
        let mut messages = chat_events
            .into_iter()
            .map(|event| {
                let pubkey = event.pubkey.to_hex();
                Ok(TrollboxMessage {
                    event_id: event.id.to_hex(),
                    npub: event
                        .pubkey
                        .to_bech32()
                        .map_err(|error| error.to_string())?,
                    display_name: profiles
                        .get(&pubkey)
                        .cloned()
                        .unwrap_or_else(|| "napstr-user".into()),
                    pubkey,
                    content: sanitise_public_chat_content(&event.content),
                    created_at: event.created_at.as_secs(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        messages.retain(|message| !message.content.is_empty());
        Ok(messages)
    }

    pub async fn send_trollbox_message(&self, content: String) -> Result<String, String> {
        self.send_public_chat_message(
            TROLLBOX_HASHTAG,
            content,
            "Public message in the Napstr trollbox",
        )
        .await
    }

    pub async fn send_track_discussion_message(
        &self,
        file_id: String,
        content: String,
    ) -> Result<String, String> {
        let topic = track_discussion_topic(&file_id)?;
        self.send_public_chat_message(
            &topic,
            content,
            "Public message in a Napstr track discussion",
        )
        .await
    }

    async fn send_public_chat_message(
        &self,
        topic: &str,
        content: String,
        alt: &str,
    ) -> Result<String, String> {
        let content = content.trim();
        let character_count = content.chars().count();
        if character_count == 0 || character_count > 500 {
            return Err("public chat messages must contain between 1 and 500 characters".into());
        }
        if content.chars().any(is_unsafe_public_chat_character) {
            return Err(
                "public chat messages must be a single line without control formatting".into(),
            );
        }
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let tags = vec![
            Tag::parse(["t", topic]),
            Tag::parse(["client", "Napstr"]),
            Tag::parse(["alt", alt]),
        ]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
        let event = client
            .sign_event_builder(
                EventBuilder::new(Kind::from(TROLLBOX_MESSAGE_KIND), content).tags(tags),
            )
            .await
            .map_err(|error| format!("public chat signing failed: {error}"))?;
        let output = client
            .send_event(&event)
            .await
            .map_err(|error| format!("public chat send failed: {error}"))?;
        if output.success.is_empty() {
            let _ = client
                .database()
                .delete(Filter::new().id(*output.id()))
                .await;
            return Err(relay_failure(
                "public chat send was rejected",
                &output.failed,
            ));
        }
        client
            .database()
            .save_event(&event)
            .await
            .map_err(|error| format!("could not save the sent public chat message: {error}"))?;
        if topic == TROLLBOX_HASHTAG {
            let _ = self.cache_trollbox_event(event.clone()).await;
        }
        let _ = self.app_handle.emit(PUBLIC_CHAT_EVENT, topic.to_string());
        Ok(event.id.to_hex())
    }

    async fn hydrate_trollbox_cache(&self, client: &Client) -> Result<(), String> {
        let db_path = self.db_path.clone();
        let events = tokio::task::spawn_blocking(move || load_trollbox_cache(&db_path))
            .await
            .map_err(|error| format!("trollbox cache task failed: {error}"))??;
        for event in events {
            let _ = client.database().save_event(&event).await;
        }
        Ok(())
    }

    async fn cache_trollbox_event(&self, event: Event) -> Result<(), String> {
        let _cache_guard = self.trollbox_cache_lock.lock().await;
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = super::open_connection(&db_path)?;
            persist_trollbox_event(&mut connection, &event)
        })
        .await
        .map_err(|error| format!("trollbox cache task failed: {error}"))?
    }

    async fn publish_profile_with_client(
        &self,
        client: &Client,
        public_key: PublicKey,
    ) -> Result<(), String> {
        let connection = super::open_connection(&self.db_path)?;
        let display_name = super::get_setting(&connection, "display_name")?;
        let about = super::get_setting(&connection, "profile_about")?;
        let picture = super::get_setting(&connection, "profile_picture")?;
        let published_fingerprint = super::get_setting(&connection, "profile_event_fingerprint")?;
        drop(connection);
        let fingerprint = profile_fingerprint(public_key, &display_name, &about, &picture);
        if published_fingerprint == fingerprint {
            return Ok(());
        }
        let mut metadata = Metadata::new()
            .name(display_name.clone())
            .display_name(display_name)
            .about(about);
        if !picture.trim().is_empty() {
            metadata = metadata.picture(
                Url::parse(&picture)
                    .map_err(|error| format!("invalid profile picture URL: {error}"))?,
            );
        }
        client
            .set_metadata(&metadata)
            .await
            .map_err(|error| format!("profile publication failed: {error}"))?;
        super::open_connection(&self.db_path)?
            .execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES('profile_event_fingerprint',?1)",
                [&fingerprint],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn publish_availability(&self) -> Result<(), String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let db_path = self.db_path.clone();
        let ids = tauri::async_runtime::spawn_blocking(move || {
            Ok::<_, String>(
                load_publish_files(&db_path)?
                    .into_iter()
                    .map(|file| file.0)
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|error| format!("availability preparation task failed: {error}"))??;
        let expiration = (Utc::now().timestamp() + 10 * 60).to_string();
        let batches: Vec<&[String]> = if ids.is_empty() {
            vec![&[]]
        } else {
            ids.chunks(400).collect()
        };
        for (index, batch) in batches.into_iter().enumerate() {
            let batch_id = format!("availability-{index:04}");
            let tags = vec![
                Tag::parse(["d", batch_id.as_str()]),
                Tag::parse(["t", "napstr-availability"]),
                Tag::parse(["expiration", expiration.as_str()]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            client
                .send_event_builder(
                    EventBuilder::new(
                        Kind::from(AVAILABILITY_KIND),
                        serde_json::to_string(batch).map_err(|error| error.to_string())?,
                    )
                    .tags(tags),
                )
                .await
                .map_err(|error| format!("availability heartbeat failed: {error}"))?;
            tokio::time::sleep(CATALOGUE_EVENT_PACE).await;
        }
        Ok(())
    }

    async fn publish_availability_delta(
        &self,
        client: &Client,
        ids: &[String],
    ) -> Result<(), String> {
        let expiration = (Utc::now().timestamp() + 10 * 60).to_string();
        let delta_id = Uuid::new_v4().to_string();
        for (index, batch) in ids.chunks(400).enumerate() {
            let batch_id = format!("availability-delta-{delta_id}-{index:04}");
            let tags = vec![
                Tag::parse(["d", batch_id.as_str()]),
                Tag::parse(["t", "napstr-availability"]),
                Tag::parse(["expiration", expiration.as_str()]),
            ]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
            client
                .send_event_builder(
                    EventBuilder::new(
                        Kind::from(AVAILABILITY_KIND),
                        serde_json::to_string(batch).map_err(|error| error.to_string())?,
                    )
                    .tags(tags),
                )
                .await
                .map_err(|error| format!("incremental availability failed: {error}"))?;
            tokio::time::sleep(CATALOGUE_EVENT_PACE).await;
        }
        Ok(())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<CatalogueResult>, String> {
        let browse = query.trim().is_empty().then_some((
            None,
            EMPTY_SEARCH_PAGE_LIMIT,
            EMPTY_SEARCH_RESULT_LIMIT,
        ));
        self.search_inner(query, browse)
            .await
            .map(|(results, _, _)| results)
    }

    pub async fn browse(
        &self,
        cursor: Option<CatalogueBrowseCursor>,
        limit: usize,
        cache_limit: usize,
    ) -> Result<CatalogueBrowsePage, String> {
        let (results, cursor, total_available) = self
            .search_inner("", Some((cursor, limit, cache_limit)))
            .await?;
        Ok(CatalogueBrowsePage {
            results,
            cursor,
            total_available,
        })
    }

    pub async fn search_audiobooks(&self, query: &str) -> Result<Vec<AudiobookResult>, String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let query = query.trim();
        // Treat the singular/plural media name as a type browse rather than a
        // literal title query. This makes searches for "audiobook" and
        // "audiobooks" return the audiobook catalogue itself.
        let query = if query.eq_ignore_ascii_case("audiobook")
            || query.eq_ignore_ascii_case("audiobooks")
        {
            ""
        } else {
            query
        };
        let mut filters = Vec::new();
        if query.is_empty() {
            filters.push(
                Filter::new()
                    .kind(Kind::from(AUDIOBOOK_KIND))
                    .hashtag("napstr-audiobook")
                    .limit(AUDIOBOOK_RESULT_LIMIT),
            );
        } else {
            filters.push(
                Filter::new()
                    .kind(Kind::from(AUDIOBOOK_KIND))
                    .search(query)
                    .limit(AUDIOBOOK_RESULT_LIMIT),
            );
            filters.extend(
                catalogue_search_tokens(&[query])
                    .into_iter()
                    .take(CATALOGUE_QUERY_TOKEN_LIMIT)
                    .map(|token| {
                        Filter::new()
                            .kind(Kind::from(AUDIOBOOK_KIND))
                            .custom_tag(SingleLetterTag::lowercase(Alphabet::T), token)
                            .limit(AUDIOBOOK_RESULT_LIMIT)
                    }),
            );
        }
        let manifest_query = stream::iter(filters)
            .map(|filter| {
                let client = client.clone();
                async move {
                    client
                        .fetch_events(filter, Duration::from_secs(8))
                        .await
                        .map_err(|error| error.to_string())
                }
            })
            .buffer_unordered(CATALOGUE_QUERY_TOKEN_LIMIT + 2)
            .collect::<Vec<_>>();
        let availability_query = self.availability_snapshot(&client);
        let (manifest_results, availability) = tokio::join!(manifest_query, availability_query);
        let mut events_by_id = HashMap::new();
        let mut failures = Vec::new();
        for result in manifest_results {
            match result {
                Ok(events) => {
                    for event in events.iter() {
                        events_by_id.insert(event.id, event.clone());
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        if events_by_id.is_empty() && !failures.is_empty() {
            return Err(format!("audiobook search failed: {}", failures[0]));
        }
        let availability = availability
            .map_err(|error| format!("audiobook availability search failed: {error}"))?;
        let online = &availability.online;
        let mut connection = super::open_connection(&self.db_path)?;
        let blocked_files = load_blocked_values(&connection, "blocked_files", "file_id")?;
        let blocked_pubkeys = load_blocked_values(&connection, "blocked_pubkeys", "pubkey")?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut aggregated: HashMap<String, AudiobookResult> = HashMap::new();
        for event in events_by_id.values() {
            let Ok(content) = serde_json::from_str::<AudiobookContent>(&event.content) else {
                continue;
            };
            if !valid_audiobook_event(event, &content)
                || content
                    .chapters
                    .iter()
                    .any(|chapter| blocked_files.contains(&chapter.file_id))
                || (!query.is_empty()
                    && !super::search_matches(
                        query,
                        &content
                            .chapters
                            .iter()
                            .flat_map(|chapter| [chapter.title.as_str(), chapter.filename.as_str()])
                            .chain([
                                content.title.as_str(),
                                content.author.as_str(),
                                content.narrator.as_str(),
                            ])
                            .collect::<Vec<_>>(),
                    ))
            {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            if blocked_pubkeys.contains(&pubkey)
                || !content
                    .chapters
                    .iter()
                    .all(|chapter| online.contains(&(pubkey.clone(), chapter.file_id.clone())))
            {
                continue;
            }
            let source = CatalogueSource {
                pubkey: pubkey.clone(),
                npub: event.pubkey.to_bech32().unwrap_or_else(|_| pubkey.clone()),
                display_name: short_key(&pubkey),
                relay: String::new(),
                about: String::new(),
                picture: String::new(),
                event_id: event.id.to_hex(),
            };
            let content_json =
                serde_json::to_string(&content).map_err(|error| error.to_string())?;
            transaction.execute(
                "INSERT OR REPLACE INTO remote_audiobooks(audiobook_id,source_pubkey,content,event_id,seen_at) VALUES(?1,?2,?3,?4,?5)",
                params![content.audiobook_id, pubkey, content_json, event.id.to_hex(), Utc::now().to_rfc3339()],
            ).map_err(|error| error.to_string())?;
            for chapter in &content.chapters {
                transaction.execute(
                    "INSERT OR REPLACE INTO remote_catalogue(file_id,source_pubkey,filename,title,artist,album,format,mime,size,license,description,tags,event_id,seen_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'unspecified','','audiobook',?10,?11)",
                    params![chapter.file_id, pubkey, chapter.filename, chapter.title, content.author, content.title, chapter.format, chapter.mime, chapter.size as i64, event.id.to_hex(), Utc::now().to_rfc3339()],
                ).map_err(|error| error.to_string())?;
            }
            aggregated
                .entry(content.audiobook_id.clone())
                .and_modify(|book| {
                    if !book
                        .sources
                        .iter()
                        .any(|existing| existing.pubkey == source.pubkey)
                    {
                        book.sources.push(source.clone());
                    }
                })
                .or_insert(AudiobookResult {
                    audiobook_id: content.audiobook_id,
                    title: content.title,
                    author: content.author,
                    narrator: content.narrator,
                    total_size: content.total_size,
                    chapters: content.chapters,
                    sources: vec![source],
                    local: false,
                    local_folder: String::new(),
                });
        }
        transaction.commit().map_err(|error| error.to_string())?;
        let mut books = aggregated.into_values().collect::<Vec<_>>();
        books.sort_by(|left, right| {
            right
                .sources
                .len()
                .cmp(&left.sources.len())
                .then_with(|| left.title.cmp(&right.title))
        });
        Ok(books)
    }

    async fn search_inner(
        &self,
        query: &str,
        browse: Option<(Option<CatalogueBrowseCursor>, usize, usize)>,
    ) -> Result<(Vec<CatalogueResult>, Option<CatalogueBrowseCursor>, usize), String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let query = query.trim();
        let browse_result_limit = browse.as_ref().map(|(_, limit, _)| *limit);
        let mut requested_file_ids = HashSet::new();
        let mut requested_file_id_order = Vec::new();
        let mut events_by_id: HashMap<EventId, Event> = HashMap::new();
        let mut catalogue_search_error = None;
        let mut next_browse_cursor = None;
        let mut online: HashSet<(String, String)>;
        let mut available_by_file: HashMap<String, HashSet<String>>;
        let mut continuation_session: Option<(String, CatalogueBrowseSession)> = None;
        let mut initial_browse_cache_limit = EMPTY_SEARCH_RESULT_LIMIT;
        let is_initial_browse = query.is_empty()
            && browse
                .as_ref()
                .map(|(cursor, _, _)| cursor.is_none())
                .unwrap_or(true);

        if query.is_empty() {
            let (cursor, limit, cache_limit) =
                browse.unwrap_or((None, EMPTY_SEARCH_PAGE_LIMIT, EMPTY_SEARCH_RESULT_LIMIT));
            initial_browse_cache_limit = cache_limit.clamp(1, EMPTY_SEARCH_RESULT_LIMIT);
            if let Some(cursor) = cursor {
                Uuid::parse_str(&cursor.session_id)
                    .map_err(|_| "invalid catalogue browse cursor".to_string())?;
                let mut sessions = self.catalogue_browse_sessions.lock().await;
                sessions.retain(|_, session| {
                    session.created_at.elapsed() < CATALOGUE_BROWSE_SESSION_LIFETIME
                });
                let mut session = sessions.remove(&cursor.session_id).ok_or_else(|| {
                    "catalogue browse session expired; run the empty search again".to_string()
                })?;
                drop(sessions);
                let page_size = limit.clamp(1, EMPTY_SEARCH_PAGE_LIMIT);
                while requested_file_id_order.len() < page_size {
                    let Some(file_id) = session.pending_file_ids.pop_front() else {
                        break;
                    };
                    requested_file_ids.insert(file_id.clone());
                    requested_file_id_order.push(file_id);
                }
                online = session.online.clone();
                available_by_file = session.available_by_file.clone();
                match fetch_catalogue_identifiers(&client, &requested_file_id_order).await {
                    Ok((events, retry_file_ids)) => {
                        for event in events {
                            events_by_id.insert(event.id, event);
                        }
                        session.pending_file_ids.extend(retry_file_ids);
                    }
                    Err(error) => {
                        for file_id in requested_file_id_order.iter().rev() {
                            session.pending_file_ids.push_front(file_id.clone());
                        }
                        self.catalogue_browse_sessions
                            .lock()
                            .await
                            .insert(cursor.session_id, session);
                        return Err(error);
                    }
                }
                continuation_session = Some((cursor.session_id, session));
            } else {
                let availability = self.availability_snapshot(&client).await?;
                online = availability.online.clone();
                available_by_file = availability.available_by_file.clone();
            }
        } else {
            let availability_query = self.availability_snapshot(&client);
            let mut filters = vec![catalogue_name_search_filter(query)];
            filters.extend(catalogue_tag_search_filters(query));
            let search_query = stream::iter(filters)
                .map(|filter| {
                    let client = client.clone();
                    async move {
                        client
                            .fetch_events(filter, Duration::from_secs(8))
                            .await
                            .map_err(|error| error.to_string())
                    }
                })
                .buffer_unordered(CATALOGUE_QUERY_TOKEN_LIMIT + 1)
                .collect::<Vec<_>>();
            let (availability, fetched) = tokio::join!(availability_query, search_query);
            let mut successful_queries = 0usize;
            let mut failures = Vec::new();
            for result in fetched {
                match result {
                    Ok(events) => {
                        successful_queries += 1;
                        for event in events.iter() {
                            events_by_id.insert(event.id, event.clone());
                        }
                    }
                    Err(error) => failures.push(error),
                }
            }
            if successful_queries == 0 {
                catalogue_search_error = Some(format!(
                    "catalogue search failed: {}",
                    failures
                        .first()
                        .map(String::as_str)
                        .unwrap_or("all relay queries failed")
                ));
            }
            let availability = availability?;
            online = availability.online.clone();
            available_by_file = availability.available_by_file.clone();
            let authors = events_by_id
                .values()
                .map(|event| event.pubkey)
                .collect::<HashSet<_>>();
            if !authors.is_empty() {
                let targeted_limit = (authors.len() * 8).clamp(8, AVAILABILITY_QUERY_LIMIT);
                if let Ok(targeted) = client
                    .fetch_events(
                        availability_search_filter()
                            .authors(authors)
                            .limit(targeted_limit),
                        Duration::from_secs(5),
                    )
                    .await
                {
                    merge_availability_events(targeted.iter(), &mut online, &mut available_by_file);
                }
            }
        }

        let mut aggregated: HashMap<String, CatalogueResult> = HashMap::new();
        let connection = super::open_connection(&self.db_path)?;
        let blocked_files = {
            let mut statement = connection
                .prepare("SELECT file_id FROM blocked_files")
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<HashSet<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        let blocked_pubkeys = {
            let mut statement = connection
                .prepare("SELECT pubkey FROM blocked_pubkeys")
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<HashSet<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        let mut total_available = available_by_file
            .iter()
            .filter(|(file_id, sources)| {
                !blocked_files.contains(*file_id)
                    && sources
                        .iter()
                        .any(|source| !blocked_pubkeys.contains(source))
            })
            .count();
        if let Some((_, session)) = &continuation_session {
            total_available = session.total_available;
        } else if is_initial_browse {
            let mut ranked_ids = available_by_file
                .iter()
                .filter(|(file_id, sources)| {
                    !blocked_files.contains(*file_id)
                        && sources
                            .iter()
                            .any(|source| !blocked_pubkeys.contains(source))
                })
                .map(|(file_id, sources)| {
                    let active_sources = sources
                        .iter()
                        .filter(|source| !blocked_pubkeys.contains(*source))
                        .count();
                    (file_id.clone(), active_sources)
                })
                .collect::<Vec<_>>();
            ranked_ids.sort_by(|(left_id, left_sources), (right_id, right_sources)| {
                right_sources
                    .cmp(left_sources)
                    .then_with(|| left_id.cmp(right_id))
            });
            requested_file_id_order = ranked_ids
                .into_iter()
                .take(initial_browse_cache_limit)
                .map(|(file_id, _)| file_id)
                .collect();
            requested_file_ids.extend(requested_file_id_order.iter().cloned());
            available_by_file.retain(|file_id, _| requested_file_ids.contains(file_id));
            online.retain(|(_, file_id)| requested_file_ids.contains(file_id));
        }

        // Previously verified relay events are an acceleration cache, not the
        // source of availability truth. Only rows paired with a fresh heartbeat
        // are eligible, and named searches are verified locally again.
        let cached = {
            let mut statement = connection
                .prepare(
                    "SELECT file_id,source_pubkey,filename,title,artist,album,format,mime,size,tags,event_id
                 FROM remote_catalogue ORDER BY seen_at DESC LIMIT ?1",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([CATALOGUE_CACHE_SCAN_LIMIT as i64], |row| {
                    let size = row.get::<_, i64>(8)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        size,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.filter_map(|row| match row {
                Ok((
                    file_id,
                    source_pubkey,
                    filename,
                    title,
                    artist,
                    album,
                    format,
                    mime,
                    size,
                    tags,
                    event_id,
                )) if size >= 0 => Some(Ok(CachedCatalogueRecord {
                    file_id,
                    source_pubkey,
                    filename,
                    title,
                    artist,
                    album,
                    format,
                    mime,
                    size: size as u64,
                    tags,
                    event_id,
                })),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
        };
        let mut cached_source_pairs = HashSet::new();
        for cached in cached {
            if blocked_files.contains(&cached.file_id)
                || blocked_pubkeys.contains(&cached.source_pubkey)
                || !online.contains(&(cached.source_pubkey.clone(), cached.file_id.clone()))
                || (!query.is_empty()
                    && !super::search_matches(
                        query,
                        &[
                            &cached.filename,
                            &cached.title,
                            &cached.artist,
                            &cached.album,
                            &cached.tags,
                        ],
                    ))
                || (query.is_empty() && !requested_file_ids.contains(&cached.file_id))
                || !valid_file_id(&cached.file_id)
                || cached.size == 0
                || !audio_claim_valid(&cached.filename, &cached.format, &cached.mime)
                || !valid_catalogue_metadata(&[
                    &cached.filename,
                    &cached.title,
                    &cached.artist,
                    &cached.album,
                    &cached.tags,
                ])
            {
                continue;
            }
            let Ok(catalogue_tags) = super::normalise_tags(&cached.tags) else {
                continue;
            };
            let Ok(public_key) = PublicKey::from_str(&cached.source_pubkey) else {
                continue;
            };
            let source = CatalogueSource {
                pubkey: cached.source_pubkey.clone(),
                npub: public_key
                    .to_bech32()
                    .unwrap_or_else(|_| cached.source_pubkey.clone()),
                display_name: short_key(&cached.source_pubkey),
                relay: String::new(),
                about: String::new(),
                picture: String::new(),
                event_id: cached.event_id,
            };
            cached_source_pairs.insert((cached.source_pubkey.clone(), cached.file_id.clone()));
            merge_catalogue_result(
                &mut aggregated,
                CatalogueContent {
                    protocol: "napstr/1".into(),
                    file_id: cached.file_id,
                    filename: cached.filename.clone(),
                    title: cached.title,
                    artist: cached.artist,
                    album: cached.album,
                    format: cached.format,
                    mime: cached.mime,
                    size: cached.size,
                    license: "unspecified".into(),
                    description: String::new(),
                    tags: catalogue_tags.clone(),
                },
                catalogue_tags,
                source,
            );
        }

        for event in events_by_id.values() {
            let Ok(content) = serde_json::from_str::<CatalogueContent>(&event.content) else {
                continue;
            };
            if !valid_catalogue_event(event, &content)
                || content.protocol != "napstr/1"
                || !valid_file_id(&content.file_id)
                || content.size == 0
                || !audio_claim_valid(&content.filename, &content.format, &content.mime)
                || !valid_catalogue_metadata(&[
                    &content.filename,
                    &content.title,
                    &content.artist,
                    &content.album,
                    &content.tags,
                ])
            {
                continue;
            }
            let Ok(catalogue_tags) = super::normalise_tags(&content.tags) else {
                continue;
            };
            if !super::search_matches(
                query,
                &[
                    &content.filename,
                    &content.title,
                    &content.artist,
                    &content.album,
                    &catalogue_tags,
                ],
            ) {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            if blocked_files.contains(&content.file_id) || blocked_pubkeys.contains(&pubkey) {
                continue;
            }
            if !online.contains(&(pubkey.clone(), content.file_id.clone())) {
                continue;
            }
            let npub = event.pubkey.to_bech32().unwrap_or_else(|_| pubkey.clone());
            let source = CatalogueSource {
                pubkey: pubkey.clone(),
                npub,
                display_name: short_key(&pubkey),
                relay: String::new(),
                about: String::new(),
                picture: String::new(),
                event_id: event.id.to_hex(),
            };
            let catalogue_name = content.filename.clone();
            connection.execute(
                "INSERT OR REPLACE INTO remote_catalogue (file_id,source_pubkey,filename,title,artist,album,format,mime,size,license,description,tags,event_id,seen_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![content.file_id, pubkey, catalogue_name, content.title, content.artist, content.album, content.format, content.mime, content.size as i64, "unspecified", "", catalogue_tags, event.id.to_hex(), Utc::now().to_rfc3339()],
            ).map_err(|error| error.to_string())?;
            cached_source_pairs.insert((pubkey, content.file_id.clone()));
            merge_catalogue_result(&mut aggregated, content, catalogue_tags, source);
        }
        if is_initial_browse {
            let pending_file_ids = requested_file_id_order
                .iter()
                .filter(|file_id| {
                    available_by_file.get(*file_id).is_some_and(|sources| {
                        sources.iter().any(|source| {
                            !blocked_pubkeys.contains(source)
                                && !cached_source_pairs
                                    .contains(&(source.clone(), (*file_id).clone()))
                        })
                    })
                })
                .cloned()
                .collect::<VecDeque<_>>();
            if !pending_file_ids.is_empty() {
                let session_id = Uuid::new_v4().to_string();
                let session = CatalogueBrowseSession {
                    created_at: Instant::now(),
                    online,
                    available_by_file,
                    pending_file_ids,
                    total_available,
                };
                let mut sessions = self.catalogue_browse_sessions.lock().await;
                sessions.retain(|_, session| {
                    session.created_at.elapsed() < CATALOGUE_BROWSE_SESSION_LIFETIME
                });
                if sessions.len() >= CATALOGUE_BROWSE_SESSION_LIMIT {
                    if let Some(oldest) = sessions
                        .iter()
                        .max_by_key(|(_, session)| session.created_at.elapsed())
                        .map(|(session_id, _)| session_id.clone())
                    {
                        sessions.remove(&oldest);
                    }
                }
                sessions.insert(session_id.clone(), session);
                next_browse_cursor = Some(CatalogueBrowseCursor { session_id });
            }
        } else if let Some((session_id, session)) = continuation_session {
            if !session.pending_file_ids.is_empty() {
                self.catalogue_browse_sessions
                    .lock()
                    .await
                    .insert(session_id.clone(), session);
                next_browse_cursor = Some(CatalogueBrowseCursor { session_id });
            }
        }
        let mut results: Vec<_> = aggregated.into_values().collect();
        if results.is_empty() {
            if let Some(error) = catalogue_search_error {
                return Err(error);
            }
        }
        let profile_keys = (!query.is_empty())
            .then(|| {
                results
                    .iter()
                    .flat_map(|result| result.sources.iter())
                    .filter_map(|source| PublicKey::from_str(&source.pubkey).ok())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .take(128)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let profiles: HashMap<String, Metadata> = stream::iter(profile_keys)
            .map(|public_key| {
                let client = client.clone();
                async move {
                    client
                        .fetch_metadata(public_key, Duration::from_secs(3))
                        .await
                        .ok()
                        .flatten()
                        .map(|metadata| (public_key.to_hex(), metadata))
                }
            })
            .buffer_unordered(16)
            .filter_map(|profile| async move { profile })
            .collect()
            .await;
        for result in &mut results {
            for source in &mut result.sources {
                if let Some(metadata) = profiles.get(&source.pubkey) {
                    if let Some(name) = metadata
                        .display_name
                        .clone()
                        .or_else(|| metadata.name.clone())
                    {
                        source.display_name = name;
                    }
                    source.about = metadata.about.clone().unwrap_or_default();
                    source.picture = metadata.picture.clone().unwrap_or_default();
                }
            }
        }
        results.sort_by(|left, right| {
            right
                .sources
                .len()
                .cmp(&left.sources.len())
                .then_with(|| left.filename.cmp(&right.filename))
        });
        results.truncate(if is_initial_browse {
            initial_browse_cache_limit
        } else if let Some(limit) = browse_result_limit {
            limit.clamp(1, EMPTY_SEARCH_PAGE_LIMIT)
        } else {
            NETWORK_SEARCH_RESULT_LIMIT
        });
        Ok((results, next_browse_cursor, total_available))
    }

    pub async fn request_download(
        &self,
        file_id: String,
        source_pubkeys: Vec<String>,
        destination_folder: Option<String>,
    ) -> Result<String, String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let mut unique = source_pubkeys
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        unique.sort();
        // A small race finds a responsive onion without opening a data stream
        // to every profile advertising the file. The remaining candidates are
        // automatic fallbacks if the selected source fails.
        unique.truncate(MAX_SEEDER_CANDIDATES);
        if unique.is_empty() {
            return Err("at least one seeder is required".into());
        }
        let tor = self.transfers.clone();
        tokio::spawn(async move {
            let _ = tor.warm_tor().await;
        });
        let connection = super::open_connection(&self.db_path)?;
        let already_local: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE file_id=?1)",
                [&file_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if already_local {
            return Err("this audio is already on this computer; play it locally".into());
        }
        let file_blocked: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
                [&file_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if file_blocked {
            return Err("this file hash is blocked".into());
        }
        let mut receivers = Vec::new();
        let mut file_record = None;
        for source in &unique {
            let source_blocked: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM blocked_pubkeys WHERE pubkey=?1)",
                    [source],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if source_blocked {
                return Err(format!("seeder {source} is blocked"));
            }
            let record: Option<(String, i64)> = connection.query_row(
                "SELECT filename,size FROM remote_catalogue WHERE file_id=?1 AND source_pubkey=?2", params![file_id, source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(|error| error.to_string())?;
            let record = record
                .ok_or_else(|| format!("seeder {source} is not in the current catalogue cache"))?;
            file_record.get_or_insert(record);
            receivers.push((
                source.clone(),
                PublicKey::from_str(source)
                    .map_err(|error| format!("invalid seeder public key: {error}"))?,
            ));
        }
        let (filename, size) = file_record.ok_or("catalogue record disappeared")?;
        let destination_folder = destination_folder
            .map(|value| {
                value
                    .chars()
                    .map(|character| {
                        if character.is_control()
                            || matches!(
                                character,
                                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                            )
                        {
                            '_'
                        } else {
                            character
                        }
                    })
                    .collect::<String>()
                    .trim_matches(|character: char| character == '.' || character.is_whitespace())
                    .chars()
                    .take(100)
                    .collect::<String>()
            })
            .filter(|value| !value.is_empty());
        let request_id = Uuid::new_v4().to_string();
        connection.execute(
            "INSERT INTO network_downloads (request_id,file_id,source_pubkey,filename,size,progress,status,speed,destination,onion,updated_at,destination_folder) VALUES (?1,?2,?3,?4,?5,0,'Racing responsive Tor seeders','—','','',?6,?7)",
            params![request_id, file_id, unique[0], filename, size, Utc::now().to_rfc3339(), destination_folder.unwrap_or_default()],
        ).map_err(|error| error.to_string())?;
        for (source, _) in &receivers {
            connection.execute("INSERT INTO download_sources(request_id,source_pubkey,status,updated_at) VALUES(?1,?2,'Requested',?3)", params![request_id, source, Utc::now().to_rfc3339()]).map_err(|error| error.to_string())?;
        }
        drop(connection);
        // A download can be requested by Napstrfy rather than by the desktop
        // UI. Wake the UI immediately so it discovers the new database row and
        // starts its normal high-frequency progress polling.
        let _ = self.app_handle.emit(TRANSFERS_CHANGED_EVENT, ());
        let message = SignalMessage::DownloadRequest {
            protocol: "napstr/1".into(),
            request_id: request_id.clone(),
            file_id,
        };
        let content = serde_json::to_string(&message).map_err(|error| error.to_string())?;
        let deliveries = stream::iter(receivers.into_iter().map(|(source, receiver)| {
            let client = client.clone();
            let content = content.clone();
            async move {
                let result = client
                    .send_private_msg(receiver, content, signal_tags())
                    .await;
                (source, result)
            }
        }))
        .buffer_unordered(MAX_SEEDER_CANDIDATES)
        .collect::<Vec<_>>()
        .await;
        let mut sent = 0;
        for (source, result) in deliveries {
            match result {
                Ok(_) => sent += 1,
                Err(error) => {
                    if let Ok(connection) = super::open_connection(&self.db_path) {
                        let _ = connection.execute("UPDATE download_sources SET status=?1,updated_at=?2 WHERE request_id=?3 AND source_pubkey=?4", params![format!("Failed: {error}"), Utc::now().to_rfc3339(), request_id, source]);
                    }
                }
            }
        }
        if sent == 0 {
            super::open_connection(&self.db_path)?
                .execute(
                    "UPDATE network_downloads SET status='Failed: NIP-17 request could not be delivered',updated_at=?1 WHERE request_id=?2",
                    params![Utc::now().to_rfc3339(), request_id],
                )
                .map_err(|error| error.to_string())?;
            let _ = self.app_handle.emit(TRANSFERS_CHANGED_EVENT, ());
            return Err("NIP-17 request could not be delivered to any seeder".into());
        }
        Ok(request_id)
    }

    pub async fn report_catalogue(
        &self,
        file_id: String,
        source_pubkey: String,
        event_id: String,
        report_type: String,
        reason: String,
    ) -> Result<(), String> {
        let report_type = report_type.trim().to_ascii_lowercase();
        if !matches!(
            report_type.as_str(),
            "illegal" | "malware" | "spam" | "nudity" | "profanity" | "impersonation" | "other"
        ) {
            return Err("unsupported NIP-56 report type".into());
        }
        if reason.trim().is_empty() || reason.len() > 500 {
            return Err("a report reason between 1 and 500 characters is required".into());
        }
        PublicKey::from_str(&source_pubkey).map_err(|_| "invalid seeder public key")?;
        if !hex::decode(&file_id)
            .map(|bytes| bytes.len() == 32)
            .unwrap_or(false)
            || !hex::decode(&event_id)
                .map(|bytes| bytes.len() == 32)
                .unwrap_or(false)
        {
            return Err("invalid event or file hash".into());
        }
        let connection = super::open_connection(&self.db_path)?;
        let known: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_catalogue WHERE file_id=?1 AND source_pubkey=?2 AND event_id=?3)",
            params![file_id, source_pubkey, event_id], |row| row.get(0),
        ).map_err(|error| error.to_string())?;
        if !known {
            return Err("the catalogue event is no longer in the local search cache".into());
        }
        drop(connection);
        let tags = vec![
            Tag::parse(["p", source_pubkey.as_str(), report_type.as_str()]),
            Tag::parse(["e", event_id.as_str(), report_type.as_str()]),
            Tag::parse(["x", file_id.as_str(), report_type.as_str()]),
            Tag::parse(["client", "Napstr"]),
        ]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
        self.client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?
            .send_event_builder(EventBuilder::new(Kind::from(1984), reason.trim()).tags(tags))
            .await
            .map_err(|error| format!("NIP-56 report publication failed: {error}"))?;
        Ok(())
    }

    async fn handle_signal(&self, sender: PublicKey, content: &str) -> Result<(), String> {
        let message: SignalMessage =
            serde_json::from_str(content).map_err(|error| error.to_string())?;
        let sender_hex = sender.to_hex();
        match message {
            SignalMessage::DownloadRequest {
                protocol,
                request_id,
                file_id,
            } if protocol == "napstr/1" => {
                let response = match self
                    .transfers
                    .create_offer(request_id.clone(), file_id.clone(), sender_hex)
                    .await
                {
                    Ok(offer) => SignalMessage::DownloadOffer {
                        protocol: "napstr/1".into(),
                        offer,
                    },
                    Err(reason) => SignalMessage::DownloadRefused {
                        protocol: "napstr/1".into(),
                        request_id,
                        file_id,
                        reason,
                    },
                };
                self.client
                    .read()
                    .await
                    .clone()
                    .ok_or("Nostr disconnected")?
                    .send_private_msg(
                        sender,
                        serde_json::to_string(&response).map_err(|error| error.to_string())?,
                        signal_tags(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            SignalMessage::DownloadOffer { protocol, offer } if protocol == "napstr/1" => {
                let connection = super::open_connection(&self.db_path)?;
                let blocked: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1) OR EXISTS(SELECT 1 FROM blocked_pubkeys WHERE pubkey=?2)",
                    params![offer.file_id, sender.to_hex()], |row| row.get(0),
                ).map_err(|error| error.to_string())?;
                if blocked {
                    return Err("offer rejected by the local blocklist".into());
                }
                let expected: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM download_sources s JOIN network_downloads d ON d.request_id=s.request_id WHERE s.request_id=?1 AND d.file_id=?2 AND s.source_pubkey=?3)",
                    params![offer.request_id, offer.file_id, sender.to_hex()], |row| row.get(0),
                ).map_err(|error| error.to_string())?;
                if !expected {
                    return Err("offer sender did not match a requested seeder".into());
                }
                connection.execute("UPDATE download_sources SET status='Connected',updated_at=?1 WHERE request_id=?2 AND source_pubkey=?3", params![Utc::now().to_rfc3339(), offer.request_id, sender.to_hex()]).map_err(|error| error.to_string())?;
                drop(connection);
                self.transfers.accept_offer(offer, sender.to_hex()).await?;
            }
            SignalMessage::DownloadRefused {
                protocol,
                request_id,
                reason,
                ..
            } if protocol == "napstr/1" => {
                let connection = super::open_connection(&self.db_path)?;
                connection.execute("UPDATE download_sources SET status=?1,updated_at=?2 WHERE request_id=?3 AND source_pubkey=?4", params![format!("Refused: {reason}"), Utc::now().to_rfc3339(), request_id, sender.to_hex()]).map_err(|error| error.to_string())?;
                let pending: i64 = connection.query_row("SELECT count(*) FROM download_sources WHERE request_id=?1 AND status IN ('Requested','Connected')", [&request_id], |row| row.get(0)).map_err(|error| error.to_string())?;
                if pending == 0 {
                    connection.execute("UPDATE network_downloads SET status='All seeders refused',updated_at=?1 WHERE request_id=?2", params![Utc::now().to_rfc3339(), request_id]).map_err(|error| error.to_string())?;
                }
            }
            _ => return Err("unsupported Napstr signal".into()),
        }
        Ok(())
    }
}

fn trollbox_filter(limit: usize) -> Filter {
    public_chat_filter(TROLLBOX_HASHTAG, limit)
}

fn nostr_client(keys: Keys, proxy: Option<SocketAddr>) -> Client {
    let database = MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: Some(LIVE_NOSTR_EVENT_LIMIT),
    });
    let mut builder = Client::builder().signer(keys).database(database);
    if let Some(address) = proxy {
        builder = builder.opts(ClientOptions::new().connection(
            RelayConnection::new()
                .proxy(address)
                .target(ConnectionTarget::All),
        ));
    }
    builder.build()
}

fn public_chat_filter(topic: &str, limit: usize) -> Filter {
    Filter::new()
        .kind(Kind::from(TROLLBOX_MESSAGE_KIND))
        .hashtag(topic)
        .limit(limit)
}

fn public_chat_topic(event: &Event) -> Option<String> {
    if event.kind != Kind::from(TROLLBOX_MESSAGE_KIND) {
        return None;
    }
    event.tags.iter().find_map(|tag| {
        if tag.kind() != TagKind::t() {
            return None;
        }
        let topic = tag.content()?;
        if topic == TROLLBOX_HASHTAG {
            return Some(topic.to_string());
        }
        let file_id = topic.strip_prefix(TRACK_DISCUSSION_PREFIX)?;
        if file_id.len() == 64
            && file_id
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            Some(topic.to_ascii_lowercase())
        } else {
            None
        }
    })
}

fn valid_cached_trollbox_event(event: &Event) -> bool {
    event.verify().is_ok()
        && public_chat_topic(event).as_deref() == Some(TROLLBOX_HASHTAG)
        && !sanitise_public_chat_content(&event.content).is_empty()
        && event.as_json().len() <= 64 * 1024
}

fn persist_trollbox_event(connection: &mut Connection, event: &Event) -> Result<(), String> {
    if !valid_cached_trollbox_event(event) {
        return Err("refusing to cache an invalid trollbox event".into());
    }
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO trollbox_events(event_id,pubkey,event_json,created_at)
             VALUES(?1,?2,?3,?4)",
            params![
                event.id.to_hex(),
                event.pubkey.to_hex(),
                event.as_json(),
                event.created_at.as_secs() as i64
            ],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM trollbox_events WHERE event_id IN (
               SELECT event_id FROM trollbox_events
               ORDER BY created_at DESC,event_id DESC LIMIT -1 OFFSET ?1
             )",
            [TROLLBOX_CACHE_LIMIT as i64],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn load_trollbox_cache(db_path: &PathBuf) -> Result<Vec<Event>, String> {
    let connection = super::open_connection(db_path)?;
    let rows = {
        let mut statement = connection
            .prepare(
                "SELECT event_id,event_json FROM trollbox_events
                 ORDER BY created_at DESC,event_id DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([TROLLBOX_CACHE_LIMIT as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        rows
    };
    let mut events = Vec::new();
    for (event_id, json) in rows {
        match Event::from_json(json)
            .ok()
            .filter(|event| event.id.to_hex() == event_id)
            .filter(valid_cached_trollbox_event)
        {
            Some(event) => events.push(event),
            None => {
                let _ =
                    connection.execute("DELETE FROM trollbox_events WHERE event_id=?1", [event_id]);
            }
        }
    }
    Ok(events)
}

fn track_discussion_topic(file_id: &str) -> Result<String, String> {
    let file_id = file_id.trim().to_ascii_lowercase();
    if file_id.len() != 64
        || !file_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err("track discussion requires a valid SHA-256 file ID".into());
    }
    Ok(format!("{TRACK_DISCUSSION_PREFIX}{file_id}"))
}

fn blocked_pubkeys(db_path: &PathBuf) -> Result<HashSet<String>, String> {
    let connection = super::open_connection(db_path)?;
    let mut statement = connection
        .prepare("SELECT pubkey FROM blocked_pubkeys")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())
}

fn safe_trollbox_name(value: &str) -> String {
    let name = value.trim();
    if name.is_empty() || name.chars().any(is_unsafe_public_chat_character) {
        return "napstr-user".into();
    }
    name.chars().take(40).collect()
}

fn load_blocked_values(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<HashSet<String>, String> {
    let allowed = matches!(
        (table, column),
        ("blocked_files", "file_id") | ("blocked_pubkeys", "pubkey")
    );
    if !allowed {
        return Err("invalid blocked-value query".into());
    }
    let mut statement = connection
        .prepare(&format!("SELECT {column} FROM {table}"))
        .map_err(|error| error.to_string())?;
    let values = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(values)
}

fn is_unsafe_public_chat_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

fn sanitise_public_chat_content(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| {
            if matches!(character, '\n' | '\r' | '\t') {
                Some(' ')
            } else if is_unsafe_public_chat_character(character) {
                None
            } else {
                Some(character)
            }
        })
        .take(500)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn relay_failure(label: &str, failures: &HashMap<RelayUrl, String>) -> String {
    let details = failures
        .values()
        .next()
        .map(String::as_str)
        .unwrap_or("the relay did not accept the event");
    format!("{label}: {details}")
}

pub fn initialise_network_schema(connection: &Connection) -> Result<(), String> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_catalogue (
           file_id TEXT NOT NULL, source_pubkey TEXT NOT NULL, filename TEXT NOT NULL, title TEXT NOT NULL,
           artist TEXT NOT NULL, album TEXT NOT NULL, format TEXT NOT NULL, mime TEXT NOT NULL, size INTEGER NOT NULL,
           license TEXT NOT NULL, event_id TEXT NOT NULL, seen_at TEXT NOT NULL,
           PRIMARY KEY(file_id, source_pubkey)
         );
         CREATE TABLE IF NOT EXISTS network_downloads (
           request_id TEXT PRIMARY KEY, file_id TEXT NOT NULL, source_pubkey TEXT NOT NULL, filename TEXT NOT NULL,
           size INTEGER NOT NULL, progress REAL NOT NULL, status TEXT NOT NULL, speed TEXT NOT NULL,
           destination TEXT NOT NULL, onion TEXT NOT NULL, updated_at TEXT NOT NULL,
           destination_folder TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS download_sources (
           request_id TEXT NOT NULL, source_pubkey TEXT NOT NULL, status TEXT NOT NULL, updated_at TEXT NOT NULL,
           PRIMARY KEY(request_id, source_pubkey),
           FOREIGN KEY(request_id) REFERENCES network_downloads(request_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS published_catalogue (
           file_id TEXT PRIMARY KEY, published_at TEXT NOT NULL, fingerprint TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS published_audiobooks (
           folder TEXT PRIMARY KEY, audiobook_id TEXT NOT NULL,
           fingerprint TEXT NOT NULL, published_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_audiobooks (
           audiobook_id TEXT NOT NULL, source_pubkey TEXT NOT NULL,
           content TEXT NOT NULL, event_id TEXT NOT NULL, seen_at TEXT NOT NULL,
           PRIMARY KEY(audiobook_id, source_pubkey)
         );
         CREATE TABLE IF NOT EXISTS trollbox_events (
           event_id TEXT PRIMARY KEY, pubkey TEXT NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS trollbox_events_recent
           ON trollbox_events(created_at DESC,event_id DESC);"
    ).map_err(|error| error.to_string())
    .and_then(|_| super::ensure_column(connection, "remote_catalogue", "description", "TEXT NOT NULL DEFAULT ''"))
    .and_then(|_| super::ensure_column(connection, "remote_catalogue", "tags", "TEXT NOT NULL DEFAULT ''"))
    .and_then(|_| super::ensure_column(connection, "published_catalogue", "fingerprint", "TEXT NOT NULL DEFAULT ''"))
    .and_then(|_| super::ensure_column(connection, "network_downloads", "destination_folder", "TEXT NOT NULL DEFAULT ''"))
}

pub fn load_network_transfers(connection: &Connection) -> Result<Vec<super::Transfer>, String> {
    // Keep the queue in creation order. `updated_at` changes with every
    // progress update and made concurrently downloading rows jump around.
    let mut statement = connection.prepare("SELECT rowid,file_id,filename,size,progress,status,speed,destination FROM network_downloads ORDER BY rowid DESC").map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(super::Transfer {
                id: -row.get::<_, i64>(0)?,
                file_id: row.get(1)?,
                filename: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                progress: row.get(4)?,
                status: row.get(5)?,
                speed: row.get(6)?,
                destination: row.get(7)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

const BACKUP_SCRYPT_LOG_N: u8 = 16;

pub fn encrypt_identity_backup(keys: &Keys, passphrase: &str) -> Result<String, String> {
    encrypt_identity_backup_with_log_n(keys, passphrase, BACKUP_SCRYPT_LOG_N)
}

fn encrypt_identity_backup_with_log_n(
    keys: &Keys,
    passphrase: &str,
    log_n: u8,
) -> Result<String, String> {
    EncryptedSecretKey::new(keys.secret_key(), passphrase, log_n, KeySecurity::Medium)
        .map_err(|error| error.to_string())?
        .to_bech32()
        .map_err(|error| error.to_string())
}

pub fn decrypt_identity_backup(ncryptsec: &str, passphrase: &str) -> Result<Keys, String> {
    let encrypted = EncryptedSecretKey::from_bech32(ncryptsec)
        .map_err(|_| "this is not a Napstr account backup".to_string())?;
    let secret = encrypted
        .decrypt(passphrase)
        .map_err(|_| "wrong passphrase for this backup".to_string())?;
    Ok(Keys::new(secret))
}

pub fn export_identity(passphrase: &str) -> Result<String, String> {
    let keys = load_or_create_identity()?;
    encrypt_identity_backup(&keys, passphrase)
}

const KEYRING_SERVICE: &str = "social.napstr.desktop";

/// Reads the identity already on this computer without creating one. `load_or_create_identity`
/// would mint a key as a side effect, which would make "is there an account here?" a
/// destructive question to ask.
pub fn current_identity_npub() -> Option<String> {
    let keys = if let Ok(nsec) = std::env::var("NAPSTR_NSEC") {
        Keys::parse(&nsec).ok()?
    } else {
        let account = profile_keyring_account(std::env::var("NAPSTR_PROFILE").ok().as_deref()).ok()?;
        let secret = Entry::new(KEYRING_SERVICE, &account).ok()?.get_password().ok()?;
        Keys::parse(&secret).ok()?
    };
    keys.public_key().to_bech32().ok()
}

/// Copies the identity currently in the keyring to a second, dated keyring entry so that
/// replacing it is recoverable. Returns the archived account's npub and keyring slot, or
/// `None` when there was no identity to preserve.
pub fn archive_current_identity() -> Result<Option<(String, String)>, String> {
    let base = profile_keyring_account(std::env::var("NAPSTR_PROFILE").ok().as_deref())?;
    let Ok(nsec) = Entry::new(KEYRING_SERVICE, &base)
        .map_err(|error| error.to_string())?
        .get_password()
    else {
        return Ok(None);
    };
    let keys = Keys::parse(&nsec).map_err(|error| error.to_string())?;
    let npub = keys
        .public_key()
        .to_bech32()
        .map_err(|error| error.to_string())?;
    let slot = format!("{base}-archived-{}", Utc::now().timestamp());
    Entry::new(KEYRING_SERVICE, &slot)
        .map_err(|error| error.to_string())?
        .set_password(&nsec)
        .map_err(|error| {
            format!("could not preserve the account being replaced in the operating-system keyring: {error}")
        })?;
    Ok(Some((npub, slot)))
}

/// Promotes a previously archived identity back to the active one. The caller archives the
/// current identity first, so switching back and forth never destroys either side.
pub fn adopt_archived_identity(slot: &str) -> Result<String, String> {
    let nsec = Entry::new(KEYRING_SERVICE, slot)
        .map_err(|error| error.to_string())?
        .get_password()
        .map_err(|_| "that archived account is no longer in the operating-system keyring".to_string())?;
    let keys = Keys::parse(&nsec).map_err(|error| error.to_string())?;
    store_identity(&keys)?;
    keys.public_key()
        .to_bech32()
        .map_err(|error| error.to_string())
}

/// Decrypts a backup purely to report whose account it holds. Stores nothing, so
/// the caller can name the account being replaced before anything is overwritten.
pub fn preview_identity_backup(ncryptsec: &str, passphrase: &str) -> Result<String, String> {
    decrypt_identity_backup(ncryptsec, passphrase)?
        .public_key()
        .to_bech32()
        .map_err(|error| error.to_string())
}

pub fn import_identity(ncryptsec: &str, passphrase: &str) -> Result<String, String> {
    let keys = decrypt_identity_backup(ncryptsec, passphrase)?;
    store_identity(&keys)?;
    keys.public_key()
        .to_bech32()
        .map_err(|error| error.to_string())
}

fn store_identity(keys: &Keys) -> Result<(), String> {
    let account = profile_keyring_account(std::env::var("NAPSTR_PROFILE").ok().as_deref())?;
    let entry = Entry::new(KEYRING_SERVICE, &account).map_err(|error| error.to_string())?;
    let nsec = keys
        .secret_key()
        .to_bech32()
        .map_err(|error| error.to_string())?;
    entry.set_password(&nsec).map_err(|error| {
        format!("could not store the restored identity in the operating-system keyring: {error}")
    })
}

fn load_or_create_identity() -> Result<Keys, String> {
    if let Ok(nsec) = std::env::var("NAPSTR_NSEC") {
        return Keys::parse(&nsec).map_err(|error| error.to_string());
    }
    let account = profile_keyring_account(std::env::var("NAPSTR_PROFILE").ok().as_deref())?;
    let entry = Entry::new(KEYRING_SERVICE, &account).map_err(|error| error.to_string())?;
    if let Ok(secret) = entry.get_password() {
        return Keys::parse(&secret).map_err(|error| error.to_string());
    }
    let keys = Keys::generate();
    let nsec = keys
        .secret_key()
        .to_bech32()
        .map_err(|error| error.to_string())?;
    entry.set_password(&nsec).map_err(|error| {
        format!("could not store Nostr identity in the operating-system keyring: {error}")
    })?;
    Ok(keys)
}

fn profile_keyring_account(profile: Option<&str>) -> Result<String, String> {
    let Some(profile) = profile else {
        return Ok("nostr-identity".into());
    };
    let profile = profile.trim();
    if profile.is_empty()
        || profile.len() > 32
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "NAPSTR_PROFILE must contain 1-32 ASCII letters, numbers, hyphens, or underscores"
                .into(),
        );
    }
    Ok(format!("nostr-identity-{profile}"))
}

fn profile_fingerprint(
    public_key: PublicKey,
    display_name: &str,
    about: &str,
    picture: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        public_key.to_hex(),
        display_name.into(),
        about.into(),
        picture.into(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    hex::encode(digest.finalize())
}

type PublishFile = (
    String,
    String,
    u64,
    String,
    String,
    String,
    String,
    String,
    String,
);
type PublishFileRecord = (
    String,
    String,
    u64,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
);

fn publish_file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublishFileRecord> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get::<_, i64>(2)? as u64,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn current_publish_file(record: PublishFileRecord) -> Option<PublishFile> {
    let (
        file_id,
        filename,
        size,
        format,
        mime,
        path,
        tags,
        indexed_modified_ns,
        title,
        artist,
        album,
    ) = record;
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() != size || super::modified_ns(&metadata) != indexed_modified_ns {
        return None;
    }
    Some((
        file_id, filename, size, format, mime, tags, title, artist, album,
    ))
}

fn load_publish_files(db_path: &PathBuf) -> Result<Vec<PublishFile>, String> {
    let connection = super::open_connection(db_path)?;
    let mut statement = connection.prepare(
        "SELECT file_id, filename, size, format, mime, path, tags, modified_ns, title, artist, album
         FROM files WHERE NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)
         ORDER BY filename,file_id"
    ).map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], publish_file_from_row)
        .map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    for row in rows {
        if let Some(file) = current_publish_file(row.map_err(|error| error.to_string())?) {
            files.push(file);
        }
    }
    Ok(files)
}

fn load_publish_files_by_id(
    db_path: &PathBuf,
    file_ids: &HashSet<String>,
) -> Result<Vec<PublishFile>, String> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    let connection = super::open_connection(db_path)?;
    let mut statement = connection
        .prepare(
            "SELECT file_id, filename, size, format, mime, path, tags, modified_ns, title, artist, album
             FROM files
             WHERE file_id=?1
               AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)",
        )
        .map_err(|error| error.to_string())?;
    let mut files = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        let record = statement
            .query_row([file_id], publish_file_from_row)
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(file) = record.and_then(current_publish_file) {
            files.push(file);
        }
    }
    files.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    Ok(files)
}

fn load_published_fingerprints(db_path: &PathBuf) -> Result<HashMap<String, String>, String> {
    let connection = super::open_connection(db_path)?;
    let mut statement = connection
        .prepare("SELECT file_id,fingerprint FROM published_catalogue")
        .map_err(|error| error.to_string())?;
    let entries = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(entries.into_iter().collect())
}

fn load_published_fingerprints_by_id(
    db_path: &PathBuf,
    file_ids: &HashSet<String>,
) -> Result<HashMap<String, String>, String> {
    if file_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let connection = super::open_connection(db_path)?;
    let mut statement = connection
        .prepare("SELECT fingerprint FROM published_catalogue WHERE file_id=?1")
        .map_err(|error| error.to_string())?;
    let mut fingerprints = HashMap::with_capacity(file_ids.len());
    for file_id in file_ids {
        if let Some(fingerprint) = statement
            .query_row([file_id], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|error| error.to_string())?
        {
            fingerprints.insert(file_id.clone(), fingerprint);
        }
    }
    Ok(fingerprints)
}

fn load_published_audiobooks(
    connection: &Connection,
) -> Result<HashMap<String, (String, String)>, String> {
    let mut statement = connection
        .prepare("SELECT folder,audiobook_id,fingerprint FROM published_audiobooks")
        .map_err(|error| error.to_string())?;
    let values = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(values)
}

fn relay_urls(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|relay| relay.starts_with("wss://") || relay.starts_with("ws://"))
        .map(str::to_owned)
        .collect()
}

fn short_key(value: &str) -> String {
    format!(
        "{}…{}",
        &value[..8.min(value.len())],
        &value[value.len().saturating_sub(4)..]
    )
}

fn audio_claim_valid(filename: &str, format: &str, mime: &str) -> bool {
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        (
            extension.as_str(),
            format.to_ascii_uppercase().as_str(),
            mime
        ),
        ("mp3", "MP3", "audio/mpeg")
            | ("flac", "FLAC", "audio/flac")
            | ("wav", "WAV", "audio/wav")
            | ("ogg", "OGG", "audio/ogg")
            | ("opus", "OPUS", "audio/ogg")
    )
}

fn signal_tags() -> Vec<Tag> {
    vec![
        Tag::expiration(Timestamp::from((Utc::now().timestamp() + 20 * 60) as u64)),
        Tag::client("Napstr"),
    ]
}

fn mime_for_format(format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previewing_a_backup_names_the_account_without_storing_it() {
        let keys = Keys::generate();
        let backup = encrypt_identity_backup_with_log_n(&keys, "correct horse battery", 10).unwrap();

        let named = preview_identity_backup(&backup, "correct horse battery").unwrap();

        assert_eq!(named, keys.public_key().to_bech32().unwrap());
        assert!(preview_identity_backup(&backup, "wrong passphrase").is_err());
    }

    #[test]
    fn identity_backup_round_trips_only_with_the_right_passphrase() {
        let keys = Keys::generate();

        // log_n 10 keeps the suite fast; the production wrapper only changes the work factor.
        let backup =
            encrypt_identity_backup_with_log_n(&keys, "correct horse battery", 10).unwrap();

        assert!(backup.starts_with("ncryptsec1"));
        let restored = decrypt_identity_backup(&backup, "correct horse battery").unwrap();
        assert_eq!(restored.public_key(), keys.public_key());
        assert!(decrypt_identity_backup(&backup, "wrong passphrase").is_err());
        assert!(decrypt_identity_backup("nsec1notabackup", "correct horse battery").is_err());
    }

    #[test]
        fn catalogue_search_filters_are_server_side_and_bounded() {
        let named = serde_json::to_value(catalogue_name_search_filter("metallica")).unwrap();
        assert_eq!(named["kinds"], serde_json::json!([CATALOGUE_KIND]));
        assert_eq!(named["#t"], serde_json::json!(["napstr"]));
        assert_eq!(named["search"], "metallica");
        assert_eq!(named["limit"], NETWORK_SEARCH_RESULT_LIMIT);

        let indexed = catalogue_tag_search_filters("Metallica Enter Sandman")
            .into_iter()
            .map(|filter| serde_json::to_value(filter).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            indexed
                .iter()
                .map(|filter| filter["#t"][0].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["metallica", "sandman", "enter"]
        );
        assert!(indexed
            .iter()
            .all(|filter| filter["limit"] == NETWORK_SEARCH_RESULT_LIMIT));
        let many_words = format!(
            "{}.mp3",
            (0..40)
                .map(|index| format!("word{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let bounded_tokens = catalogue_search_tokens(&[&many_words]);
        assert_eq!(bounded_tokens.len(), CATALOGUE_SEARCH_TOKEN_LIMIT);
        let balanced_tokens = catalogue_search_tokens(&[
            &many_words,
            "A Distinct Track Title",
            "Specific Artist",
            "Recognisable Album",
            "favourite",
        ]);
        for expected in [
            "distinct",
            "track",
            "title",
            "specific",
            "artist",
            "recognisable",
            "album",
            "favourite",
        ] {
            assert!(balanced_tokens.contains(&expected.to_string()));
        }
        assert_eq!(
            catalogue_search_tokens(&["The House of the Rising Sun"]),
            vec!["house", "rising", "sun"]
        );
        assert_eq!(
            catalogue_search_tokens(&["enter_sandman.mp3", "enter-sandman"]),
            vec!["enter", "sandman"]
        );

        let identifiers = vec!["a".repeat(64), "b".repeat(64)];
        let browse = serde_json::to_value(catalogue_identifier_filter(&identifiers)).unwrap();
        assert_eq!(browse["kinds"], serde_json::json!([CATALOGUE_KIND]));
        assert_eq!(browse["#t"], serde_json::json!(["napstr"]));
        assert_eq!(browse["#d"], serde_json::json!(identifiers));
        assert_eq!(browse["limit"], 16);

        let availability = serde_json::to_value(availability_search_filter()).unwrap();
        assert_eq!(
            availability["kinds"],
            serde_json::json!([AVAILABILITY_KIND])
        );
        assert_eq!(
            availability["limit"],
            serde_json::json!(AVAILABILITY_QUERY_LIMIT)
        );

        let old_empty_fingerprint = String::new();
        let tokens = vec!["metallica".to_string()];
        let fingerprint = catalogue_event_fingerprint("catalogue", &tokens);
        assert!(!catalogue_publication_is_current(
            Some(&old_empty_fingerprint),
            &fingerprint
        ));
        assert!(catalogue_publication_is_current(
            Some(&fingerprint),
            &fingerprint
        ));
        assert_ne!(
            fingerprint,
            catalogue_event_fingerprint("catalogue", &["sandman".to_string()])
        );
    }

    #[test]
    fn audiobook_manifests_bind_order_and_reject_paths() {
        let chapters = vec![
            AudiobookChapter {
                position: 1,
                file_id: "11".repeat(32),
                filename: "01 - Start.mp3".into(),
                title: "Start".into(),
                format: "MP3".into(),
                mime: "audio/mpeg".into(),
                size: 10,
            },
            AudiobookChapter {
                position: 2,
                file_id: "22".repeat(32),
                filename: "02 - Finish.mp3".into(),
                title: "Finish".into(),
                format: "MP3".into(),
                mime: "audio/mpeg".into(),
                size: 20,
            },
        ];
        let id = audiobook_id(&chapters);
        let content = AudiobookContent {
            protocol: "napstr/1".into(),
            audiobook_id: id.clone(),
            title: "A Book".into(),
            author: "An Author".into(),
            narrator: String::new(),
            total_size: 30,
            chapters,
        };
        let keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::from(AUDIOBOOK_KIND),
            serde_json::to_string(&content).unwrap(),
        )
        .tags(vec![
            Tag::identifier(id.clone()),
            Tag::hashtag("napstr-audiobook"),
        ])
        .sign_with_keys(&keys)
        .unwrap();
        assert!(valid_audiobook_event(&event, &content));

        let single_chapter = content.chapters[0].clone();
        let single_id = audiobook_id(std::slice::from_ref(&single_chapter));
        let single_content = AudiobookContent {
            protocol: "napstr/1".into(),
            audiobook_id: single_id.clone(),
            title: "A Complete Book in One File".into(),
            author: "An Author".into(),
            narrator: String::new(),
            total_size: single_chapter.size,
            chapters: vec![single_chapter],
        };
        let single_event = EventBuilder::new(
            Kind::from(AUDIOBOOK_KIND),
            serde_json::to_string(&single_content).unwrap(),
        )
        .tags(vec![
            Tag::identifier(single_id),
            Tag::hashtag("napstr-audiobook"),
        ])
        .sign_with_keys(&keys)
        .unwrap();
        assert!(valid_audiobook_event(&single_event, &single_content));

        let mut unsafe_content = content.clone();
        unsafe_content.chapters[0].filename = "../secret.mp3".into();
        let unsafe_event = EventBuilder::new(
            Kind::from(AUDIOBOOK_KIND),
            serde_json::to_string(&unsafe_content).unwrap(),
        )
        .tags(vec![Tag::identifier(id), Tag::hashtag("napstr-audiobook")])
        .sign_with_keys(&keys)
        .unwrap();
        assert!(!valid_audiobook_event(&unsafe_event, &unsafe_content));
    }

    #[test]
    fn trollbox_uses_a_separate_public_chat_kind_and_indexed_topic() {
        let filter = serde_json::to_value(trollbox_filter(TROLLBOX_CACHE_LIMIT)).unwrap();
        assert_eq!(filter["kinds"], serde_json::json!([9]));
        assert_eq!(filter["#t"], serde_json::json!(["napstr-trollbox"]));
        assert_eq!(filter["limit"], serde_json::json!(TROLLBOX_CACHE_LIMIT));
        assert_eq!(safe_trollbox_name("  Alice  "), "Alice");
        assert_eq!(safe_trollbox_name("bad\nname"), "napstr-user");
        assert_eq!(safe_trollbox_name("Alice\u{202e}resu"), "napstr-user");
        assert_eq!(
            sanitise_public_chat_content("hello\nworld\u{202e}"),
            "hello world"
        );
        assert_eq!(sanitise_public_chat_content(&"x".repeat(501)).len(), 500);
        let event = EventBuilder::new(Kind::from(TROLLBOX_MESSAGE_KIND), "hello")
            .tag(Tag::hashtag(TROLLBOX_HASHTAG))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert_eq!(public_chat_topic(&event).as_deref(), Some(TROLLBOX_HASHTAG));
    }

    #[tokio::test]
    async fn public_chat_events_are_queryable_without_a_relay_echo() {
        let keys = Keys::generate();
        let client = nostr_client(keys.clone(), None);
        let event = EventBuilder::new(Kind::from(TROLLBOX_MESSAGE_KIND), "hello")
            .tag(Tag::hashtag(TROLLBOX_HASHTAG))
            .sign_with_keys(&keys)
            .unwrap();

        let status = client.database().save_event(&event).await.unwrap();
        assert!(status.is_success());
        let events = client
            .database()
            .query(trollbox_filter(TROLLBOX_CACHE_LIMIT))
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events.first().map(|event| event.id), Some(event.id));
    }

    #[test]
    fn catalogue_schema_migrates_publication_fingerprints() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE published_catalogue (
                   file_id TEXT PRIMARY KEY, published_at TEXT NOT NULL
                 );
                 INSERT INTO published_catalogue(file_id,published_at) VALUES('abc','now');",
            )
            .unwrap();

        initialise_network_schema(&connection).unwrap();

        let fingerprint: String = connection
            .query_row(
                "SELECT fingerprint FROM published_catalogue WHERE file_id='abc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fingerprint.is_empty());
    }

    #[test]
    fn trollbox_cache_retains_only_the_newest_200_verified_events() {
        let mut connection = Connection::open_in_memory().unwrap();
        initialise_network_schema(&connection).unwrap();
        let keys = Keys::generate();
        for index in 0..205 {
            let event = EventBuilder::new(
                Kind::from(TROLLBOX_MESSAGE_KIND),
                format!("message {index}"),
            )
            .tag(Tag::hashtag(TROLLBOX_HASHTAG))
            .custom_created_at(Timestamp::from(index))
            .sign_with_keys(&keys)
            .unwrap();
            persist_trollbox_event(&mut connection, &event).unwrap();
        }
        let count: i64 = connection
            .query_row("SELECT count(*) FROM trollbox_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, TROLLBOX_CACHE_LIMIT as i64);
        let oldest: i64 = connection
            .query_row("SELECT min(created_at) FROM trollbox_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(oldest, 5);
    }

    #[test]
    fn track_discussions_use_the_sha256_as_an_indexed_public_chat_topic() {
        let file_id = "ab".repeat(32);
        let topic = track_discussion_topic(&file_id).unwrap();
        assert_eq!(topic, format!("napstr-{file_id}"));
        let filter = serde_json::to_value(public_chat_filter(&topic, 100)).unwrap();
        assert_eq!(filter["kinds"], serde_json::json!([9]));
        assert_eq!(filter["#t"], serde_json::json!([topic]));
        assert!(track_discussion_topic("not-a-sha256").is_err());
    }

    #[test]
    fn test_profiles_use_separate_keyring_accounts() {
        assert_eq!(profile_keyring_account(None).unwrap(), "nostr-identity");
        assert_eq!(
            profile_keyring_account(Some("alice")).unwrap(),
            "nostr-identity-alice"
        );
        assert_eq!(
            profile_keyring_account(Some("bob_2")).unwrap(),
            "nostr-identity-bob_2"
        );
        assert!(profile_keyring_account(Some("../alice")).is_err());
        assert!(profile_keyring_account(Some("")).is_err());
    }

    #[test]
    fn profile_publication_fingerprint_tracks_identity_and_contents() {
        let alice = Keys::generate();
        let bob = Keys::generate();
        let original = profile_fingerprint(
            alice.public_key(),
            "napstr-user",
            "Sharing files privately with Napstr. napstr.net",
            "",
        );
        assert_eq!(
            original,
            profile_fingerprint(
                alice.public_key(),
                "napstr-user",
                "Sharing files privately with Napstr. napstr.net",
                "",
            )
        );
        assert_ne!(
            original,
            profile_fingerprint(alice.public_key(), "Alice", "Updated profile", "")
        );
        assert_ne!(
            original,
            profile_fingerprint(
                bob.public_key(),
                "napstr-user",
                "Sharing files privately with Napstr. napstr.net",
                "",
            )
        );
    }

    #[tokio::test]
    async fn nip17_signal_is_gift_wrapped_and_unwraps_for_only_the_receiver() {
        let sender = Keys::generate();
        let receiver = Keys::generate();
        let stranger = Keys::generate();
        let content = serde_json::to_string(&SignalMessage::DownloadRequest {
            protocol: "napstr/1".into(),
            request_id: "request-1".into(),
            file_id: "a".repeat(64),
        })
        .unwrap();
        let gift = EventBuilder::private_msg(
            &sender,
            receiver.public_key(),
            content.clone(),
            signal_tags(),
        )
        .await
        .unwrap();
        assert_eq!(gift.kind, Kind::GiftWrap);
        assert!(!gift.content.contains("request-1"));
        let unwrapped = Client::new(receiver).unwrap_gift_wrap(&gift).await.unwrap();
        assert_eq!(unwrapped.sender, sender.public_key());
        assert_eq!(unwrapped.rumor.kind, Kind::PrivateDirectMessage);
        assert_eq!(unwrapped.rumor.content, content);
        assert!(unwrapped.rumor.tags.expiration().is_some());
        assert!(Client::new(stranger).unwrap_gift_wrap(&gift).await.is_err());
    }
}
