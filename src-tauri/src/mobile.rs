use crate::{
    build_local_audiobooks, build_local_audiobooks_from_files, load_files, load_files_by_id,
    load_transfers, open_connection, search_matches,
};
use chrono::Utc;
use iroh::{endpoint::presets, Endpoint, SecretKey};
use napstr_remote_protocol::{
    ClientRequest, PairingTicket, RemoteAudiobook, RemoteAudiobookSummary, RemoteSource,
    RemoteTrack, RemoteTransfer, ServerResponse, ALPN, MAX_CONTROL_FRAME_BYTES, MAX_PAGE_SIZE,
    PROTOCOL_VERSION,
};
use qrcode::{render::svg, QrCode};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

const PAIRING_LIFETIME_SECONDS: i64 = 5 * 60;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    endpoint_id: String,
    name: String,
    paired_at: String,
    last_seen: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileStatus {
    running: bool,
    online: bool,
    endpoint_id: String,
    error: String,
    devices: Vec<PairedDevice>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MobilePairingOffer {
    ticket: String,
    qr_svg: String,
    expires_at: i64,
    endpoint_id: String,
}

struct PairingSession {
    token: String,
    expires_at: i64,
}

#[derive(Default)]
struct RuntimeStatus {
    running: bool,
    online: bool,
    endpoint_id: String,
    error: String,
}

struct MusicLibraryCache {
    revision: u64,
    tracks: Arc<Vec<RemoteTrack>>,
    audiobook_chapter_ids: Arc<std::collections::HashSet<String>>,
}

pub struct MobileService {
    db_path: PathBuf,
    key_path: PathBuf,
    network: Arc<crate::network::NetworkService>,
    endpoint: tokio::sync::RwLock<Option<Endpoint>>,
    start_lock: tokio::sync::Mutex<()>,
    pairing: Mutex<Option<PairingSession>>,
    status: Mutex<RuntimeStatus>,
    audiobook_cache: Mutex<std::collections::HashMap<String, RemoteAudiobook>>,
    music_library_cache: Mutex<Option<MusicLibraryCache>>,
    last_seen_updates: Mutex<std::collections::HashMap<String, Instant>>,
    connection_slots: Arc<tokio::sync::Semaphore>,
    request_slots: Arc<tokio::sync::Semaphore>,
}

impl MobileService {
    pub fn new(
        db_path: PathBuf,
        app_data: PathBuf,
        network: Arc<crate::network::NetworkService>,
    ) -> Result<Arc<Self>, String> {
        initialise_schema(&db_path)?;
        Ok(Arc::new(Self {
            db_path,
            key_path: app_data.join("iroh-identity"),
            network,
            endpoint: tokio::sync::RwLock::new(None),
            start_lock: tokio::sync::Mutex::new(()),
            pairing: Mutex::new(None),
            status: Mutex::new(RuntimeStatus::default()),
            audiobook_cache: Mutex::new(std::collections::HashMap::new()),
            music_library_cache: Mutex::new(None),
            last_seen_updates: Mutex::new(std::collections::HashMap::new()),
            connection_slots: Arc::new(tokio::sync::Semaphore::new(16)),
            request_slots: Arc::new(tokio::sync::Semaphore::new(32)),
        }))
    }

    pub fn has_devices(&self) -> bool {
        load_devices(&self.db_path)
            .map(|devices| !devices.is_empty())
            .unwrap_or(false)
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), String> {
        let _guard = self.start_lock.lock().await;
        if self.endpoint.read().await.is_some() {
            return Ok(());
        }
        if let Ok(mut status) = self.status.lock() {
            status.error.clear();
        }
        let key = load_or_create_key(&self.key_path)?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|error| self.remember_error(format!("Iroh failed to start: {error}")))?;
        let endpoint_id = endpoint.id().to_string();
        {
            let mut slot = self.endpoint.write().await;
            *slot = Some(endpoint.clone());
        }
        if let Ok(mut status) = self.status.lock() {
            status.running = true;
            status.endpoint_id = endpoint_id;
            status.error.clear();
        }

        let accept_service = self.clone();
        let accept_endpoint = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = accept_endpoint.accept().await {
                let permit = match accept_service.connection_slots.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        incoming.refuse();
                        continue;
                    }
                };
                let service = accept_service.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                        Ok(Ok(connection)) => service.handle_connection(connection).await,
                        Ok(Err(error)) => eprintln!("Napstrfy connection failed: {error}"),
                        Err(_) => eprintln!("Napstrfy connection handshake timed out"),
                    }
                });
            }
        });

        let online_service = self.clone();
        tokio::spawn(async move {
            endpoint.online().await;
            if let Ok(mut status) = online_service.status.lock() {
                status.online = true;
            }
        });
        Ok(())
    }

    pub async fn stop(&self) {
        if let Some(endpoint) = self.endpoint.write().await.take() {
            endpoint.close().await;
        }
        if let Ok(mut status) = self.status.lock() {
            status.running = false;
            status.online = false;
        }
    }

    pub async fn status(self: &Arc<Self>) -> MobileStatus {
        if self.endpoint.read().await.is_none() {
            let _ = self.start().await;
        }
        let runtime = self.status.lock().ok();
        MobileStatus {
            running: runtime.as_ref().map(|value| value.running).unwrap_or(false),
            online: runtime.as_ref().map(|value| value.online).unwrap_or(false),
            endpoint_id: runtime
                .as_ref()
                .map(|value| value.endpoint_id.clone())
                .unwrap_or_default(),
            error: runtime
                .as_ref()
                .map(|value| value.error.clone())
                .unwrap_or_else(|| "Mobile service state is unavailable".into()),
            devices: load_devices(&self.db_path).unwrap_or_default(),
        }
    }

    pub async fn create_pairing(self: &Arc<Self>) -> Result<MobilePairingOffer, String> {
        self.start().await?;
        let endpoint = self
            .endpoint
            .read()
            .await
            .clone()
            .ok_or("Iroh is not running")?;
        // Waiting briefly gives the ticket a relay path as well as the endpoint
        // identity. A DNS lookup remains available if this times out.
        let _ = tokio::time::timeout(Duration::from_secs(12), endpoint.online()).await;
        let endpoint_addr = serde_json::to_string(&endpoint.addr())
            .map_err(|error| format!("could not encode the Iroh address: {error}"))?;
        let token = hex::encode(rand::random::<[u8; 32]>());
        let expires_at = Utc::now().timestamp() + PAIRING_LIFETIME_SECONDS;
        *self
            .pairing
            .lock()
            .map_err(|_| "pairing state lock was poisoned")? = Some(PairingSession {
            token: token.clone(),
            expires_at,
        });
        let desktop_name = open_connection(&self.db_path)
            .and_then(|connection| crate::get_setting(&connection, "display_name"))
            .unwrap_or_else(|_| "Napstr".into());
        let ticket = PairingTicket {
            version: PROTOCOL_VERSION,
            endpoint_id: endpoint.id().to_string(),
            endpoint_addr,
            token,
            expires_at,
            desktop_name,
        }
        .to_uri()?;
        let qr_svg = QrCode::new(ticket.as_bytes())
            .map_err(|error| format!("could not create pairing QR: {error}"))?
            .render::<svg::Color>()
            .min_dimensions(280, 280)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build();
        Ok(MobilePairingOffer {
            endpoint_id: endpoint.id().to_string(),
            ticket,
            qr_svg,
            expires_at,
        })
    }

    pub fn revoke(&self, endpoint_id: &str) -> Result<(), String> {
        let parsed = endpoint_id
            .parse::<iroh::EndpointId>()
            .map_err(|_| "invalid Iroh endpoint ID")?;
        open_connection(&self.db_path)?
            .execute(
                "DELETE FROM mobile_devices WHERE endpoint_id=?1",
                [parsed.to_string()],
            )
            .map_err(|error| error.to_string())?;
        if let Ok(mut updates) = self.last_seen_updates.lock() {
            updates.remove(endpoint_id);
        }
        Ok(())
    }

    fn remember_error(&self, message: String) -> String {
        if let Ok(mut status) = self.status.lock() {
            status.error = message.clone();
            status.running = false;
            status.online = false;
        }
        message
    }

    async fn handle_connection(self: Arc<Self>, connection: iroh::endpoint::Connection) {
        let remote_id = connection.remote_id().to_string();
        if !self.connection_is_allowed(&remote_id) {
            return;
        }
        loop {
            if !self.connection_is_allowed(&remote_id) {
                break;
            }
            let (mut send, mut receive) =
                match tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, connection.accept_bi()).await {
                    Ok(Ok(streams)) => streams,
                    Ok(Err(_)) | Err(_) => break,
                };
            let service = self.clone();
            let remote_id = remote_id.clone();
            let permit = match self.request_slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = write_response(
                        &mut send,
                        &ServerResponse::Error {
                            message: "Napstrfy has too many simultaneous requests".into(),
                        },
                    )
                    .await;
                    let _ = send.finish();
                    continue;
                }
            };
            tokio::spawn(async move {
                let _permit = permit;
                let request =
                    match tokio::time::timeout(Duration::from_secs(15), read_request(&mut receive))
                        .await
                    {
                        Ok(Ok(request)) => request,
                        Ok(Err(error)) => {
                            let _ = write_response(
                                &mut send,
                                &ServerResponse::Error { message: error },
                            )
                            .await;
                            let _ = send.finish();
                            return;
                        }
                        Err(_) => {
                            let _ = write_response(
                                &mut send,
                                &ServerResponse::Error {
                                    message: "Napstrfy request timed out".into(),
                                },
                            )
                            .await;
                            let _ = send.finish();
                            return;
                        }
                    };
                if let Err(error) = service.serve_request(&remote_id, request, &mut send).await {
                    let _ =
                        write_response(&mut send, &ServerResponse::Error { message: error }).await;
                }
                let _ = send.finish();
            });
        }
    }

    fn connection_is_allowed(&self, remote_id: &str) -> bool {
        if self.authorise(remote_id).is_ok() {
            return true;
        }
        let now = Utc::now().timestamp();
        self.pairing
            .lock()
            .ok()
            .and_then(|mut pairing| {
                if pairing
                    .as_ref()
                    .is_some_and(|session| session.expires_at < now)
                {
                    *pairing = None;
                }
                pairing.as_ref().map(|_| true)
            })
            .unwrap_or(false)
    }

    async fn audiobook_catalogue(&self, query: &str) -> Result<Vec<RemoteAudiobook>, String> {
        let (local_books, local_tracks) = load_local_remote_audiobooks(&self.db_path)?;
        let mut books = std::collections::HashMap::new();
        for book in local_books.into_iter().filter(|book| {
            search_matches(
                query,
                &book
                    .chapters
                    .iter()
                    .flat_map(|chapter| [chapter.title.as_str(), chapter.filename.as_str()])
                    .chain([
                        book.title.as_str(),
                        book.author.as_str(),
                        book.narrator.as_str(),
                    ])
                    .collect::<Vec<_>>(),
            )
        }) {
            books.insert(book.audiobook_id.clone(), book);
        }
        for book in self.network.search_audiobooks(query).await? {
            books
                .entry(book.audiobook_id.clone())
                .or_insert_with(|| remote_audiobook(book, &local_tracks));
        }
        let mut audiobooks = books.into_values().collect::<Vec<_>>();
        audiobooks.sort_by(|left, right| left.title.cmp(&right.title));
        audiobooks.truncate(MAX_PAGE_SIZE);
        if let Ok(mut cache) = self.audiobook_cache.lock() {
            cache.clear();
            cache.extend(
                audiobooks
                    .iter()
                    .cloned()
                    .map(|book| (book.audiobook_id.clone(), book)),
            );
        }
        Ok(audiobooks)
    }

    fn music_library(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<
        (
            Vec<RemoteTrack>,
            usize,
            Arc<std::collections::HashSet<String>>,
        ),
        String,
    > {
        let revision = library_revision(&self.db_path)?;
        let cached = self.music_library_cache.lock().ok().and_then(|cache| {
            cache.as_ref().and_then(|cached| {
                (cached.revision == revision)
                    .then(|| (cached.tracks.clone(), cached.audiobook_chapter_ids.clone()))
            })
        });
        let (tracks, audiobook_chapter_ids) = match cached {
            Some(cached) => cached,
            None => {
                let (tracks, audiobook_chapter_ids) = build_music_library(&self.db_path)?;
                let tracks = Arc::new(tracks);
                let audiobook_chapter_ids = Arc::new(audiobook_chapter_ids);
                if let Ok(mut cache) = self.music_library_cache.lock() {
                    *cache = Some(MusicLibraryCache {
                        revision,
                        tracks: tracks.clone(),
                        audiobook_chapter_ids: audiobook_chapter_ids.clone(),
                    });
                }
                (tracks, audiobook_chapter_ids)
            }
        };
        let (page, total) = page_music_library(&tracks, query, offset, limit);
        Ok((page, total, audiobook_chapter_ids))
    }

    async fn serve_request(
        &self,
        remote_id: &str,
        request: ClientRequest,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<(), String> {
        if let ClientRequest::Pair { token, device_name } = request {
            self.accept_pairing(remote_id, &token, &device_name)?;
            return write_response(
                send,
                &ServerResponse::Paired {
                    desktop_name: open_connection(&self.db_path)
                        .and_then(|connection| crate::get_setting(&connection, "display_name"))
                        .unwrap_or_else(|_| "Napstr".into()),
                },
            )
            .await;
        }
        self.authorise(remote_id)?;
        self.touch_device(remote_id);
        match request {
            ClientRequest::Library {
                query,
                offset,
                limit,
            } => {
                if query.chars().count() > 120 {
                    return Err("Library searches are limited to 120 characters".into());
                }
                let (tracks, total, _) =
                    self.music_library(&query, offset, limit.clamp(1, MAX_PAGE_SIZE))?;
                write_response(send, &ServerResponse::Library { tracks, total }).await
            }
            ClientRequest::Search { query } => {
                let query = query.trim();
                if query.is_empty() || query.chars().count() > 120 {
                    return Err("Search for between 1 and 120 characters".into());
                }
                let (mut tracks, _, audiobook_chapter_ids) =
                    self.music_library(query, 0, MAX_PAGE_SIZE)?;
                let remote = self.network.search(query).await?;
                for result in remote {
                    if audiobook_chapter_ids.contains(&result.file_id)
                        || tracks.iter().any(|track| track.file_id == result.file_id)
                    {
                        continue;
                    }
                    tracks.push(RemoteTrack {
                        file_id: result.file_id,
                        filename: result.filename,
                        title: result.title,
                        artist: result.artist,
                        album: result.album,
                        format: result.format,
                        mime: result.mime,
                        size: result.size,
                        tags: result.tags,
                        local: false,
                        sources: result
                            .sources
                            .into_iter()
                            .map(|source| RemoteSource {
                                pubkey: source.pubkey,
                                display_name: source.display_name,
                            })
                            .collect(),
                    });
                }
                tracks.sort_by(|left, right| {
                    right
                        .local
                        .cmp(&left.local)
                        .then_with(|| right.sources.len().cmp(&left.sources.len()))
                        .then_with(|| left.filename.cmp(&right.filename))
                });
                tracks.truncate(MAX_PAGE_SIZE);
                write_response(send, &ServerResponse::Search { tracks }).await
            }
            ClientRequest::Audiobooks { query } => {
                let query = query.trim();
                if query.chars().count() > 120 {
                    return Err("Audiobook searches are limited to 120 characters".into());
                }
                // Legacy full response retained for older Napstrfy installs.
                let audiobooks = self.audiobook_catalogue(query).await?;
                write_response(send, &ServerResponse::Audiobooks { audiobooks }).await
            }
            ClientRequest::AudiobookLibrary {
                query,
                offset,
                limit,
            } => {
                let query = query.trim();
                if query.chars().count() > 120 {
                    return Err("Audiobook searches are limited to 120 characters".into());
                }
                let audiobooks = self.audiobook_catalogue(query).await?;
                let total = audiobooks.len();
                let summaries = audiobooks
                    .into_iter()
                    .skip(offset)
                    .take(limit.clamp(1, MAX_PAGE_SIZE))
                    .map(|book| RemoteAudiobookSummary {
                        audiobook_id: book.audiobook_id,
                        title: book.title,
                        author: book.author,
                        narrator: book.narrator,
                        total_size: book.total_size,
                        chapter_count: book.chapters.len(),
                    })
                    .collect();
                write_response(
                    send,
                    &ServerResponse::AudiobookLibrary {
                        audiobooks: summaries,
                        total,
                    },
                )
                .await
            }
            ClientRequest::Audiobook { audiobook_id } => {
                // Napstrfy may retain a library summary while Napstr restarts or
                // while another search replaces this process's detail cache.
                // Local files and audiobook configuration are authoritative, so
                // resolve those from the database before consulting the cache.
                let local_audiobook = load_local_remote_audiobooks(&self.db_path)?
                    .0
                    .into_iter()
                    .find(|book| book.audiobook_id == audiobook_id);
                if let Some(audiobook) = local_audiobook {
                    if let Ok(mut cache) = self.audiobook_cache.lock() {
                        cache.insert(audiobook_id, audiobook.clone());
                    }
                    return write_response(send, &ServerResponse::Audiobook { audiobook }).await;
                }
                let mut audiobook = self
                    .audiobook_cache
                    .lock()
                    .map_err(|_| "audiobook cache lock poisoned".to_string())?
                    .get(&audiobook_id)
                    .cloned()
                    .ok_or("That audiobook is no longer available; refresh the list")?;
                let chapter_ids = audiobook
                    .chapters
                    .iter()
                    .map(|chapter| chapter.file_id.clone())
                    .collect::<Vec<_>>();
                let local_tracks =
                    load_files_by_id(&open_connection(&self.db_path)?, &chapter_ids)?
                        .into_iter()
                        .map(|file| {
                            (
                                file.file_id.clone(),
                                RemoteTrack {
                                    file_id: file.file_id,
                                    filename: file.filename,
                                    title: file.title,
                                    artist: file.artist,
                                    album: file.album,
                                    format: file.format,
                                    mime: file.mime,
                                    size: file.size,
                                    tags: file.tags,
                                    local: true,
                                    sources: Vec::new(),
                                },
                            )
                        })
                        .collect::<std::collections::HashMap<_, _>>();
                for chapter in &mut audiobook.chapters {
                    if let Some(local) = local_tracks.get(&chapter.file_id) {
                        *chapter = local.clone();
                    }
                }
                if let Ok(mut cache) = self.audiobook_cache.lock() {
                    cache.insert(audiobook_id, audiobook.clone());
                }
                write_response(send, &ServerResponse::Audiobook { audiobook }).await
            }
            ClientRequest::RequestDownload {
                file_id,
                source_pubkeys,
                destination_folder,
            } => {
                let request_id = self
                    .network
                    .request_download(file_id, source_pubkeys, destination_folder)
                    .await?;
                write_response(send, &ServerResponse::DownloadRequested { request_id }).await
            }
            ClientRequest::Transfers => {
                let transfers = load_remote_transfers(&self.db_path)?;
                write_response(send, &ServerResponse::Transfers { transfers }).await
            }
            ClientRequest::FetchAudio { file_id } => {
                let track = local_track(&self.db_path, &file_id)?;
                let path = secure_audio_path(&self.db_path, &file_id)?;
                write_response(send, &ServerResponse::AudioReady { track }).await?;
                let mut file = tokio::fs::File::open(path)
                    .await
                    .map_err(|error| format!("could not open the audio: {error}"))?;
                let mut buffer = vec![0u8; 256 * 1024];
                loop {
                    let count = file
                        .read(&mut buffer)
                        .await
                        .map_err(|error| format!("could not read the audio: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    send.write_all(&buffer[..count])
                        .await
                        .map_err(|error| format!("Iroh audio stream failed: {error}"))?;
                }
                Ok(())
            }
            ClientRequest::Available { file_ids } => {
                if file_ids.len() > MAX_PAGE_SIZE
                    || file_ids.iter().any(|file_id| !is_sha256_file_id(file_id))
                {
                    return Err("Invalid cached-file availability request".into());
                }
                let available = load_files_by_id(&open_connection(&self.db_path)?, &file_ids)?
                    .into_iter()
                    .map(|file| file.file_id)
                    .collect();
                write_response(
                    send,
                    &ServerResponse::Available {
                        file_ids: available,
                    },
                )
                .await
            }
            ClientRequest::Status => {
                write_response(
                    send,
                    &ServerResponse::Status {
                        library_revision: library_revision(&self.db_path)?,
                    },
                )
                .await
            }
            ClientRequest::Ping => write_response(send, &ServerResponse::Pong).await,
            ClientRequest::Pair { .. } => unreachable!(),
        }
    }

    fn accept_pairing(&self, remote_id: &str, token: &str, name: &str) -> Result<(), String> {
        let now = Utc::now();
        let mut pairing = self
            .pairing
            .lock()
            .map_err(|_| "pairing state lock was poisoned")?;
        let current = pairing
            .as_ref()
            .ok_or("No pairing request is open on Napstr")?;
        if current.expires_at < now.timestamp() {
            *pairing = None;
            return Err("The pairing code has expired".into());
        }
        if current.token.as_bytes() != token.as_bytes() {
            return Err("The pairing code is not valid".into());
        }
        let endpoint = remote_id
            .parse::<iroh::EndpointId>()
            .map_err(|_| "invalid mobile Iroh identity")?;
        let name = clean_device_name(name);
        let connection = open_connection(&self.db_path)?;
        connection
            .execute(
                "INSERT INTO mobile_devices(endpoint_id,name,paired_at,last_seen)
                 VALUES(?1,?2,?3,?3)
                 ON CONFLICT(endpoint_id) DO UPDATE SET name=excluded.name,last_seen=excluded.last_seen",
                params![endpoint.to_string(), name, now.to_rfc3339()],
            )
            .map_err(|error| error.to_string())?;
        *pairing = None;
        Ok(())
    }

    fn authorise(&self, remote_id: &str) -> Result<(), String> {
        let allowed: bool = open_connection(&self.db_path)?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM mobile_devices WHERE endpoint_id=?1)",
                [remote_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if allowed {
            Ok(())
        } else {
            Err("This phone is not paired with Napstr".into())
        }
    }

    fn touch_device(&self, remote_id: &str) {
        let now = Instant::now();
        if let Ok(mut updates) = self.last_seen_updates.lock() {
            if updates
                .get(remote_id)
                .is_some_and(|updated| now.duration_since(*updated) < Duration::from_secs(60))
            {
                return;
            }
            updates.insert(remote_id.to_string(), now);
        }
        if let Ok(connection) = open_connection(&self.db_path) {
            let _ = connection.execute(
                "UPDATE mobile_devices SET last_seen=?1 WHERE endpoint_id=?2",
                params![Utc::now().to_rfc3339(), remote_id],
            );
        }
    }
}

fn is_sha256_file_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remote_audiobook(
    book: crate::network::AudiobookResult,
    local_tracks: &std::collections::HashMap<String, RemoteTrack>,
) -> RemoteAudiobook {
    let sources = book
        .sources
        .iter()
        .map(|source| RemoteSource {
            pubkey: source.pubkey.clone(),
            display_name: source.display_name.clone(),
        })
        .collect::<Vec<_>>();
    let chapters = book
        .chapters
        .into_iter()
        .map(|chapter| {
            local_tracks
                .get(&chapter.file_id)
                .cloned()
                .unwrap_or_else(|| RemoteTrack {
                    file_id: chapter.file_id,
                    filename: chapter.filename,
                    title: chapter.title,
                    artist: book.author.clone(),
                    album: book.title.clone(),
                    format: chapter.format,
                    mime: chapter.mime,
                    size: chapter.size,
                    tags: "audiobook".into(),
                    local: false,
                    sources: sources.clone(),
                })
        })
        .collect();
    RemoteAudiobook {
        audiobook_id: book.audiobook_id,
        title: book.title,
        author: book.author,
        narrator: book.narrator,
        total_size: book.total_size,
        chapters,
    }
}

fn load_local_remote_audiobooks(
    db_path: &Path,
) -> Result<
    (
        Vec<RemoteAudiobook>,
        std::collections::HashMap<String, RemoteTrack>,
    ),
    String,
> {
    let connection = open_connection(db_path)?;
    let local_tracks = load_files(&connection, None)?
        .into_iter()
        .map(|file| {
            let track = RemoteTrack {
                file_id: file.file_id.clone(),
                filename: file.filename,
                title: file.title,
                artist: file.artist,
                album: file.album,
                format: file.format,
                mime: file.mime,
                size: file.size,
                tags: file.tags,
                local: true,
                sources: Vec::new(),
            };
            (file.file_id, track)
        })
        .collect::<std::collections::HashMap<_, _>>();
    let audiobooks = build_local_audiobooks(&connection)?
        .into_iter()
        .map(|book| remote_audiobook(book, &local_tracks))
        .collect();
    Ok((audiobooks, local_tracks))
}

fn initialise_schema(db_path: &Path) -> Result<(), String> {
    open_connection(db_path)?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS mobile_devices (
               endpoint_id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               paired_at TEXT NOT NULL,
               last_seen TEXT NOT NULL
             );",
        )
        .map_err(|error| error.to_string())
}

fn load_devices(db_path: &Path) -> Result<Vec<PairedDevice>, String> {
    let connection = open_connection(db_path)?;
    let mut statement = connection
        .prepare(
            "SELECT endpoint_id,name,paired_at,last_seen FROM mobile_devices ORDER BY last_seen DESC",
        )
        .map_err(|error| error.to_string())?;
    let devices = statement
        .query_map([], |row| {
            Ok(PairedDevice {
                endpoint_id: row.get(0)?,
                name: row.get(1)?,
                paired_at: row.get(2)?,
                last_seen: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(devices)
}

#[cfg(test)]
fn load_library(
    db_path: &Path,
    query: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<RemoteTrack>, usize, std::collections::HashSet<String>), String> {
    let (tracks, audiobook_chapter_ids) = build_music_library(db_path)?;
    let (page, total) = page_music_library(&tracks, query, offset, limit);
    Ok((page, total, audiobook_chapter_ids))
}

fn build_music_library(
    db_path: &Path,
) -> Result<(Vec<RemoteTrack>, std::collections::HashSet<String>), String> {
    let connection = open_connection(db_path)?;
    let files = load_files(&connection, None)?;
    let audiobook_chapter_ids = build_local_audiobooks_from_files(&connection, &files)?
        .into_iter()
        .flat_map(|book| book.chapters.into_iter().map(|chapter| chapter.file_id))
        .collect::<std::collections::HashSet<_>>();
    let tracks = files
        .into_iter()
        .filter(|file| !audiobook_chapter_ids.contains(&file.file_id))
        .map(|file| RemoteTrack {
            file_id: file.file_id,
            filename: file.filename,
            title: file.title,
            artist: file.artist,
            album: file.album,
            format: file.format,
            mime: file.mime,
            size: file.size,
            tags: file.tags,
            local: true,
            sources: Vec::new(),
        })
        .collect::<Vec<_>>();
    Ok((tracks, audiobook_chapter_ids))
}

fn page_music_library(
    tracks: &[RemoteTrack],
    query: &str,
    offset: usize,
    limit: usize,
) -> (Vec<RemoteTrack>, usize) {
    let mut page = Vec::with_capacity(limit.min(tracks.len()));
    let mut total = 0usize;
    for track in tracks {
        if !search_matches(
            query,
            &[
                &track.filename,
                &track.title,
                &track.artist,
                &track.album,
                &track.tags,
            ],
        ) {
            continue;
        }
        if total >= offset && page.len() < limit {
            page.push(track.clone());
        }
        total += 1;
    }
    (page, total)
}

fn library_revision(db_path: &Path) -> Result<u64, String> {
    let revision = open_connection(db_path)?
        .query_row("SELECT revision FROM library_state WHERE id=1", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| error.to_string())?;
    Ok(revision.max(0) as u64)
}

fn local_track(db_path: &Path, file_id: &str) -> Result<RemoteTrack, String> {
    load_files_by_id(&open_connection(db_path)?, &[file_id.to_string()])?
        .into_iter()
        .map(|file| RemoteTrack {
            file_id: file.file_id,
            filename: file.filename,
            title: file.title,
            artist: file.artist,
            album: file.album,
            format: file.format,
            mime: file.mime,
            size: file.size,
            tags: file.tags,
            local: true,
            sources: Vec::new(),
        })
        .next()
        .ok_or("This track is no longer in the Napstr folder".into())
}

fn secure_audio_path(db_path: &Path, file_id: &str) -> Result<PathBuf, String> {
    if hex::decode(file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
        == false
    {
        return Err("invalid SHA-256 file ID".into());
    }
    let connection = open_connection(db_path)?;
    let root = crate::get_setting(&connection, "shared_folder")?;
    let path: Option<String> = connection
        .query_row(
            "SELECT path FROM files WHERE file_id=?1 AND format IN ('MP3','FLAC','WAV','OGG','OPUS')",
            [file_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let root = PathBuf::from(root)
        .canonicalize()
        .map_err(|_| "The Napstr folder is unavailable")?;
    let path = PathBuf::from(path.ok_or("This track is no longer available")?)
        .canonicalize()
        .map_err(|_| "This track is no longer available")?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err("Napstr refused a path outside the selected folder".into());
    }
    Ok(path)
}

fn load_remote_transfers(db_path: &Path) -> Result<Vec<RemoteTransfer>, String> {
    Ok(load_transfers(&open_connection(db_path)?)?
        .into_iter()
        .map(|transfer| RemoteTransfer {
            id: transfer.id.to_string(),
            file_id: transfer.file_id,
            filename: transfer.filename,
            size: transfer.size,
            progress: transfer.progress,
            status: transfer.status,
            speed: transfer.speed,
        })
        .collect())
}

fn clean_device_name(value: &str) -> String {
    let cleaned = value
        .chars()
        .filter(|character| !character.is_control() && !is_bidi_control(*character))
        .take(64)
        .collect::<String>();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Napstrfy phone".into()
    } else {
        cleaned.into()
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn load_or_create_key(path: &Path) -> Result<SecretKey, String> {
    if let Ok(bytes) = fs::read(path) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "The saved Iroh identity has an invalid length")?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let key = SecretKey::generate();
    write_private_key(path, &key.to_bytes())?;
    Ok(key)
}

#[cfg(unix)]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    match options.open(path) {
        Ok(mut file) => file.write_all(bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, bytes).map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

async fn read_request(receive: &mut iroh::endpoint::RecvStream) -> Result<ClientRequest, String> {
    let mut length = [0u8; 4];
    receive
        .read_exact(&mut length)
        .await
        .map_err(|error| format!("could not read request length: {error}"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_CONTROL_FRAME_BYTES {
        return Err("invalid Napstrfy request size".into());
    }
    let mut payload = vec![0u8; length];
    receive
        .read_exact(&mut payload)
        .await
        .map_err(|error| format!("could not read request: {error}"))?;
    serde_json::from_slice(&payload).map_err(|_| "invalid Napstrfy request".into())
}

async fn write_response(
    send: &mut iroh::endpoint::SendStream,
    response: &ServerResponse,
) -> Result<(), String> {
    let payload = serde_json::to_vec(response).map_err(|error| error.to_string())?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err("Napstrfy response is too large".into());
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .map_err(|error| error.to_string())?;
    send.write_all(&payload)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_cannot_include_control_characters() {
        assert_eq!(clean_device_name("  My\nPhone\u{202e}  "), "MyPhone");
        assert_eq!(clean_device_name("\n\r"), "Napstrfy phone");
    }

    #[test]
    fn music_library_excludes_local_audiobook_chapters() {
        let directory =
            std::env::temp_dir().join(format!("napstr-mobile-music-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("napstr.sqlite3");
        crate::initialise_database(&db_path, &directory).unwrap();
        let original_revision = library_revision(&db_path).unwrap();
        let connection = open_connection(&db_path).unwrap();
        for (file_id, filename, folder, title) in [
            ("11".repeat(32), "Song.mp3", "Music", "A Song"),
            ("22".repeat(32), "Part 1.mp3", "Audiobooks/A Book", "Part 1"),
            ("33".repeat(32), "Part 2.mp3", "Audiobooks/A Book", "Part 2"),
        ] {
            connection
                .execute(
                    "INSERT INTO files(file_id,filename,path,size,format,indexed_at,mime,folder,title,artist)
                     VALUES(?1,?2,?3,1,'MP3','now','audio/mpeg',?4,?5,'Author')",
                    params![
                        file_id,
                        filename,
                        directory.join(filename).to_string_lossy(),
                        folder,
                        title
                    ],
                )
                .unwrap();
        }
        drop(connection);
        assert!(library_revision(&db_path).unwrap() > original_revision);

        let (tracks, total, audiobook_chapter_ids) =
            load_library(&db_path, "", 0, MAX_PAGE_SIZE).unwrap();

        assert_eq!(total, 1);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "A Song");
        assert_eq!(audiobook_chapter_ids.len(), 2);
        assert!(audiobook_chapter_ids.contains(&"22".repeat(32)));
        assert!(audiobook_chapter_ids.contains(&"33".repeat(32)));

        // Audiobook detail lookup must be reconstructable from the database.
        // Napstrfy can retain summaries while the desktop process restarts, so
        // correctness cannot depend on an in-memory catalogue cache.
        let (audiobooks, _) = load_local_remote_audiobooks(&db_path).unwrap();
        assert_eq!(audiobooks.len(), 1);
        assert_eq!(audiobooks[0].title, "A Book");
        assert_eq!(audiobooks[0].chapters.len(), 2);
        assert!(audiobooks[0].chapters.iter().all(|chapter| chapter.local));
        fs::remove_dir_all(directory).unwrap();
    }
}
