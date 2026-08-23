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
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::Emitter;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

pub const CATALOGUE_KIND: u16 = 30421;
pub const AVAILABILITY_KIND: u16 = 30422;
const TROLLBOX_HASHTAG: &str = "napstr-trollbox";
const TROLLBOX_MESSAGE_KIND: u16 = 9;
const TRACK_DISCUSSION_PREFIX: &str = "napstr-";
const TRACK_DISCUSSION_SUBSCRIPTION: &str = "napstr-track-discussion";
const PUBLIC_CHAT_EVENT: &str = "napstr-public-chat";
const TROLLBOX_CACHE_LIMIT: usize = 200;
const LIVE_NOSTR_EVENT_LIMIT: usize = 35_000;
const MAX_SEEDER_CANDIDATES: usize = 3;

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

pub struct NetworkService {
    db_path: PathBuf,
    transfers: Arc<TransferService>,
    tor: Arc<TorManager>,
    app_handle: tauri::AppHandle,
    client: RwLock<Option<Client>>,
    keys: RwLock<Option<Keys>>,
    start_lock: Mutex<()>,
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

        self.publish_catalogue().await?;
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

    pub async fn publish_catalogue(&self) -> Result<usize, String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let files = load_publish_files(&self.db_path)?;
        if !files.is_empty() {
            let transfers = self.transfers.clone();
            tokio::spawn(async move {
                let _ = transfers.warm_for_sharing().await;
            });
        }
        let current_ids: HashSet<String> = files.iter().map(|file| file.0.clone()).collect();
        let stale = load_published_ids(&self.db_path)?
            .into_iter()
            .filter(|id| !current_ids.contains(id))
            .collect::<Vec<_>>();
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
            super::open_connection(&self.db_path)?
                .execute(
                    "DELETE FROM published_catalogue WHERE file_id=?1",
                    [&file_id],
                )
                .map_err(|error| error.to_string())?;
        }
        let mut published = 0;
        for (file_id, filename, size, format, mime, catalogue_tags) in files {
            let content = CatalogueContent {
                protocol: "napstr/1".into(),
                file_id: file_id.clone(),
                filename: filename.clone(),
                title: filename.clone(),
                artist: String::new(),
                album: String::new(),
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
            let tags = vec![
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
            client
                .send_event_builder(
                    EventBuilder::new(
                        Kind::from(CATALOGUE_KIND),
                        serde_json::to_string(&content).map_err(|error| error.to_string())?,
                    )
                    .tags(tags),
                )
                .await
                .map_err(|error| format!("catalogue publication failed for {filename}: {error}"))?;
            super::open_connection(&self.db_path)?.execute("INSERT OR REPLACE INTO published_catalogue(file_id,published_at) VALUES (?1,?2)", params![file_id, Utc::now().to_rfc3339()]).map_err(|error| error.to_string())?;
            published += 1;
        }
        self.publish_availability().await?;
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
        let ids = load_publish_files(&self.db_path)?
            .into_iter()
            .map(|file| file.0)
            .collect::<Vec<_>>();
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
        }
        Ok(())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<CatalogueResult>, String> {
        let client = self
            .client
            .read()
            .await
            .clone()
            .ok_or("Nostr is not connected")?;
        let catalogue_query = client.fetch_events(
            Filter::new()
                .kind(Kind::from(CATALOGUE_KIND))
                .hashtag("napstr")
                .limit(10_000),
            Duration::from_secs(10),
        );
        let availability_query = client.fetch_events(
            Filter::new()
                .kind(Kind::from(AVAILABILITY_KIND))
                .hashtag("napstr-availability")
                .since(Timestamp::from(
                    Utc::now().timestamp().saturating_sub(12 * 60) as u64,
                ))
                .limit(5_000),
            Duration::from_secs(6),
        );
        let (events, availability) = tokio::join!(catalogue_query, availability_query);
        let events = events.map_err(|error| format!("catalogue search failed: {error}"))?;
        let availability =
            availability.map_err(|error| format!("availability search failed: {error}"))?;
        let mut online: HashSet<(String, String)> = HashSet::new();
        for event in availability.iter() {
            if event
                .tags
                .expiration()
                .map(|expires| *expires <= Timestamp::now())
                .unwrap_or(true)
            {
                continue;
            }
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&event.content) {
                for id in ids {
                    online.insert((event.pubkey.to_hex(), id));
                }
            }
        }
        let mut aggregated: HashMap<String, CatalogueResult> = HashMap::new();
        let connection = super::open_connection(&self.db_path)?;
        for event in events.iter() {
            let Ok(content) = serde_json::from_str::<CatalogueContent>(&event.content) else {
                continue;
            };
            if content.protocol != "napstr/1"
                || !hex::decode(&content.file_id)
                    .map(|bytes| bytes.len() == 32)
                    .unwrap_or(false)
                || !audio_claim_valid(&content.filename, &content.format, &content.mime)
            {
                continue;
            }
            let Ok(catalogue_tags) = super::normalise_tags(&content.tags) else {
                continue;
            };
            if !super::search_matches(query, &[&content.filename, &catalogue_tags]) {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            let blocked: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1) OR EXISTS(SELECT 1 FROM blocked_pubkeys WHERE pubkey=?2)",
                params![content.file_id, pubkey], |row| row.get(0),
            ).map_err(|error| error.to_string())?;
            if blocked {
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
                params![content.file_id, pubkey, catalogue_name, catalogue_name, "", "", content.format, content.mime, content.size as i64, "unspecified", "", catalogue_tags, event.id.to_hex(), Utc::now().to_rfc3339()],
            ).map_err(|error| error.to_string())?;
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
                    filename: catalogue_name.clone(),
                    title: catalogue_name,
                    artist: String::new(),
                    album: String::new(),
                    format: content.format,
                    mime: content.mime,
                    size: content.size,
                    license: "unspecified".into(),
                    description: String::new(),
                    tags: catalogue_tags,
                    sources: vec![source],
                });
        }
        let mut results: Vec<_> = aggregated.into_values().collect();
        let profile_keys = results
            .iter()
            .flat_map(|result| result.sources.iter())
            .filter_map(|source| PublicKey::from_str(&source.pubkey).ok())
            .collect::<HashSet<_>>()
            .into_iter()
            .take(128);
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
        Ok(results)
    }

    pub async fn request_download(
        &self,
        file_id: String,
        source_pubkeys: Vec<String>,
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
        let request_id = Uuid::new_v4().to_string();
        connection.execute(
            "INSERT INTO network_downloads (request_id,file_id,source_pubkey,filename,size,progress,status,speed,destination,onion,updated_at) VALUES (?1,?2,?3,?4,?5,0,'Racing responsive Tor seeders','—','','',?6)",
            params![request_id, file_id, unique[0], filename, size, Utc::now().to_rfc3339()],
        ).map_err(|error| error.to_string())?;
        for (source, _) in &receivers {
            connection.execute("INSERT INTO download_sources(request_id,source_pubkey,status,updated_at) VALUES(?1,?2,'Requested',?3)", params![request_id, source, Utc::now().to_rfc3339()]).map_err(|error| error.to_string())?;
        }
        drop(connection);
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
           destination TEXT NOT NULL, onion TEXT NOT NULL, updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS download_sources (
           request_id TEXT NOT NULL, source_pubkey TEXT NOT NULL, status TEXT NOT NULL, updated_at TEXT NOT NULL,
           PRIMARY KEY(request_id, source_pubkey),
           FOREIGN KEY(request_id) REFERENCES network_downloads(request_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS published_catalogue (file_id TEXT PRIMARY KEY, published_at TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS trollbox_events (
           event_id TEXT PRIMARY KEY, pubkey TEXT NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS trollbox_events_recent
           ON trollbox_events(created_at DESC,event_id DESC);"
    ).map_err(|error| error.to_string())
    .and_then(|_| super::ensure_column(connection, "remote_catalogue", "description", "TEXT NOT NULL DEFAULT ''"))
    .and_then(|_| super::ensure_column(connection, "remote_catalogue", "tags", "TEXT NOT NULL DEFAULT ''"))
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

pub fn import_identity(ncryptsec: &str, passphrase: &str) -> Result<String, String> {
    let keys = decrypt_identity_backup(ncryptsec, passphrase)?;
    store_identity(&keys)?;
    keys.public_key()
        .to_bech32()
        .map_err(|error| error.to_string())
}

fn store_identity(keys: &Keys) -> Result<(), String> {
    let account = profile_keyring_account(std::env::var("NAPSTR_PROFILE").ok().as_deref())?;
    let entry = Entry::new("social.napstr.desktop", &account).map_err(|error| error.to_string())?;
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
    let entry = Entry::new("social.napstr.desktop", &account).map_err(|error| error.to_string())?;
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

type PublishFile = (String, String, u64, String, String, String);

fn load_publish_files(db_path: &PathBuf) -> Result<Vec<PublishFile>, String> {
    let connection = super::open_connection(db_path)?;
    let mut statement = connection.prepare(
        "SELECT file_id, filename, size, format, mime, path, tags
         FROM files WHERE NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)
         ORDER BY filename"
    ).map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    for row in rows {
        let (file_id, filename, size, format, mime, path, tags) =
            row.map_err(|error| error.to_string())?;
        let Ok(validated) = crate::audio::validate_audio(std::path::Path::new(&path)) else {
            continue;
        };
        if validated.format != format || validated.mime != mime {
            continue;
        }
        files.push((file_id, filename, size, format, mime, tags));
    }
    Ok(files)
}

fn load_published_ids(db_path: &PathBuf) -> Result<Vec<String>, String> {
    let connection = super::open_connection(db_path)?;
    let mut statement = connection
        .prepare("SELECT file_id FROM published_catalogue")
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(ids)
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
