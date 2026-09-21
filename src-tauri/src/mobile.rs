use crate::{
    build_local_audiobooks, build_local_audiobooks_from_files, cover_publish::CoverAlbumNote,
    cover_publish::CoverPublisher, load_files, load_files_by_id, load_transfers, open_connection,
    search_matches,
};
use chrono::Utc;
use iroh::{endpoint::presets, Endpoint, SecretKey};
use napstr_remote_protocol::{
    ClientRequest, CoverReportResult, PairingTicket, PlaybackCommand, RemoteAlbumCover,
    RemoteAudiobook, RemoteAudiobookSummary, RemoteSource, RemoteTrack, RemoteTransfer,
    ServerResponse, ALPN, MAX_CONTROL_FRAME_BYTES, MAX_COVER_KEYS, MAX_PAGE_SIZE, MAX_PLAY_QUEUE,
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
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    endpoint_id: String,
    name: String,
    paired_at: String,
    last_seen: String,
    stream_only: bool,
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
    // Legacy wire/database name: read-only host access still allows phone caching.
    stream_only: bool,
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
    /// The cover worker, so a phone's own results join the queue the window
    /// fills. The phone has no MusicBrainz client and resolves art only from
    /// kind `30427`, so an album that only it has shown would otherwise never be
    /// looked up at all.
    covers: Arc<CoverPublisher>,
    playback: Arc<crate::playback_bridge::PlaybackBridge>,
    endpoint: tokio::sync::RwLock<Option<Endpoint>>,
    start_lock: tokio::sync::Mutex<()>,
    pairing: Mutex<Vec<PairingSession>>,
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
        covers: Arc<CoverPublisher>,
        playback: Arc<crate::playback_bridge::PlaybackBridge>,
    ) -> Result<Arc<Self>, String> {
        initialise_schema(&db_path)?;
        Ok(Arc::new(Self {
            db_path,
            key_path: app_data.join("iroh-identity"),
            network,
            covers,
            playback,
            endpoint: tokio::sync::RwLock::new(None),
            start_lock: tokio::sync::Mutex::new(()),
            pairing: Mutex::new(Vec::new()),
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

    pub async fn create_pairing(
        self: &Arc<Self>,
        stream_only: bool,
    ) -> Result<MobilePairingOffer, String> {
        self.start().await?;
        if let Some(endpoint) = self.endpoint.read().await.clone() {
            // Waiting briefly gives the ticket a relay path as well as the
            // endpoint identity. A DNS lookup remains available if it times out.
            let _ = tokio::time::timeout(Duration::from_secs(12), endpoint.online()).await;
        }
        self.issue_pairing(stream_only).await
    }

    /// Mint a one-use code for the endpoint that is already running.
    ///
    /// This starts nothing on purpose: it is also reached from request handling,
    /// and a request that arrived over Iroh has already proved the endpoint is
    /// up. Calling `start` from there would make `start` reachable through its
    /// own spawned tasks, which the compiler cannot type (E0391).
    async fn issue_pairing(&self, stream_only: bool) -> Result<MobilePairingOffer, String> {
        let endpoint = self
            .endpoint
            .read()
            .await
            .clone()
            .ok_or("Iroh is not running")?;
        let endpoint_addr = serde_json::to_string(&endpoint.addr())
            .map_err(|error| format!("could not encode the Iroh address: {error}"))?;
        let token = hex::encode(rand::random::<[u8; 32]>());
        let expires_at = Utc::now().timestamp() + PAIRING_LIFETIME_SECONDS;
        {
            let mut pairing = self
                .pairing
                .lock()
                .map_err(|_| "pairing state lock was poisoned")?;
            pairing.retain(|session| {
                session.stream_only != stream_only && session.expires_at >= Utc::now().timestamp()
            });
            pairing.push(PairingSession {
                token: token.clone(),
                expires_at,
                stream_only,
            });
        }
        let desktop_name = self.desktop_name();
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

    /// The name shown to a phone. One place, so every answer agrees.
    fn desktop_name(&self) -> String {
        open_connection(&self.db_path)
            .and_then(|connection| crate::get_setting(&connection, "display_name"))
            .unwrap_or_else(|_| "Napstr".into())
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
                    // Never let an overloaded peer block the connection's accept loop.
                    let _ = send.reset(1u32.into());
                    let _ = receive.stop(1u32.into());
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
            .map(|mut pairing| {
                pairing.retain(|session| session.expires_at >= now);
                !pairing.is_empty()
            })
            .unwrap_or(false)
    }

    async fn audiobook_catalogue(
        &self,
        query: &str,
        stream_only: bool,
    ) -> Result<Vec<RemoteAudiobook>, String> {
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
        let remote = if stream_only {
            Vec::new()
        } else {
            self.network.search_audiobooks(query).await?
        };
        for book in remote {
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
        if let ClientRequest::Pair {
            token,
            device_name,
        } = request
        {
            let stream_only =
                self.accept_pairing(remote_id, &token, &device_name)?;
            return write_response(
                send,
                &ServerResponse::Paired {
                    stream_only,
                    desktop_name: self.desktop_name(),
                },
            )
            .await;
        }
        let stream_only = self.authorise(remote_id)?;
        check_request_permission(stream_only, &request)?;
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
                let remote = if stream_only {
                    Vec::new()
                } else {
                    self.network.search(query).await?
                };
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
                // Hand the albums this phone is about to see to the cover worker,
                // which is what lets a phone get art by proxy. It has no
                // MusicBrainz client of its own, so an album that only this phone
                // has shown is never looked up unless the search says so here.
                // Reporting is a local write and does nothing while both cover
                // switches are off; a failure to queue must not fail the search.
                // It is still said out loud, because a queue write that fails
                // silently is exactly how a phone ends up with no art and no
                // reason why.
                if let Err(error) = self.covers.note_visible(
                    &tracks
                        .iter()
                        .map(|track| CoverAlbumNote {
                            artist: track.artist.clone(),
                            album: track.album.clone(),
                        })
                        .collect::<Vec<_>>(),
                ) {
                    eprintln!("Could not queue the albums a phone searched for: {error}");
                }
                write_response(send, &ServerResponse::Search { tracks }).await
            }
            ClientRequest::Audiobooks { query } => {
                let query = query.trim();
                if query.chars().count() > 120 {
                    return Err("Audiobook searches are limited to 120 characters".into());
                }
                // Legacy full response retained for older Napstrfy installs.
                let audiobooks = self.audiobook_catalogue(query, stream_only).await?;
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
                let audiobooks = self.audiobook_catalogue(query, stream_only).await?;
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
                if stream_only {
                    return Err("This audiobook is not in Napstr's local library".into());
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
                    check_request_permission(
                        self.authorise(remote_id)?,
                        &ClientRequest::FetchAudio {
                            file_id: file_id.clone(),
                        },
                    )?;
                    let count = file
                        .read(&mut buffer)
                        .await
                        .map_err(|error| format!("could not read the audio: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    write_bytes(send, &buffer[..count]).await?;
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
            ClientRequest::AlbumCovers { keys } => {
                if keys.len() > MAX_COVER_KEYS {
                    return Err("Too many album covers were requested at once".into());
                }
                // The rendering view, not the assertion view: a phone should
                // see the art this computer resolved for itself, exactly as the
                // desktop's own window does. Reporting one of those is refused
                // by `report_cover`, because there is no claim to report.
                let covers = self
                    .network
                    .best_known_covers(keys)
                    .await?
                    .into_iter()
                    .map(remote_album_cover)
                    .collect();
                write_response(send, &ServerResponse::AlbumCovers { covers }).await
            }
            ClientRequest::Status => {
                write_response(
                    send,
                    &ServerResponse::Status {
                        library_revision: library_revision(&self.db_path)?,
                        cover_revision: cover_revision(&self.db_path)?,
                        stream_only,
                    },
                )
                .await
            }
            ClientRequest::Playback { command } => {
                let state = self
                    .playback
                    .apply(&self.db_path, bounded_playback(command)?)?;
                write_response(send, &ServerResponse::Playback { state }).await
            }
            ClientRequest::PlaybackState => {
                write_response(
                    send,
                    &ServerResponse::Playback {
                        state: self.playback.state(&self.db_path),
                    },
                )
                .await
            }
            ClientRequest::ReadOnlyTicket => {
                // Only a read-only code can come out of here, which is what lets
                // a phone with write access lend its access on without ever
                // widening it: whoever scans this browses and plays, no more.
                let offer = self.issue_pairing(true).await?;
                let desktop_name = self.desktop_name();
                let mut qr_svg = offer.qr_svg;
                let candidate = ServerResponse::ReadOnlyTicket {
                    uri: offer.ticket.clone(),
                    qr_svg: qr_svg.clone(),
                    expires_at: offer.expires_at,
                    desktop_name: desktop_name.clone(),
                };
                if serde_json::to_vec(&candidate)
                    .map(|encoded| encoded.len())
                    .unwrap_or(usize::MAX)
                    > MAX_CONTROL_FRAME_BYTES
                {
                    // The QR is around a hundred kilobytes of path data. Drop
                    // the image rather than failing the request that carries
                    // the code itself.
                    qr_svg.clear();
                }
                write_response(
                    send,
                    &ServerResponse::ReadOnlyTicket {
                        uri: offer.ticket,
                        qr_svg,
                        expires_at: offer.expires_at,
                        desktop_name,
                    },
                )
                .await
            }
            ClientRequest::ReportCover { key, reason, note } => {
                let report_id = self.network.report_cover(key, reason, note).await?;
                write_response(
                    send,
                    &ServerResponse::CoverReported {
                        report: CoverReportResult {
                            report_id,
                            queued: false,
                        },
                    },
                )
                .await
            }
            ClientRequest::Ping => write_response(send, &ServerResponse::Pong).await,
            ClientRequest::Pair { .. } => unreachable!(),
        }
    }

    fn accept_pairing(
        &self,
        remote_id: &str,
        token: &str,
        name: &str,
    ) -> Result<bool, String> {
        let mut pairing = self
            .pairing
            .lock()
            .map_err(|_| "pairing state lock was poisoned")?;
        accept_pairing(
            &self.db_path,
            &mut pairing,
            remote_id,
            token,
            name,
        )
    }

    fn authorise(&self, remote_id: &str) -> Result<bool, String> {
        device_stream_only(&self.db_path, remote_id)
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

fn accept_pairing(
    db_path: &Path,
    pairing: &mut Vec<PairingSession>,
    remote_id: &str,
    token: &str,
    name: &str,
) -> Result<bool, String> {
    let now = Utc::now();
    pairing.retain(|session| session.expires_at >= now.timestamp());
    let index = pairing
        .iter()
        .position(|session| session.token.as_bytes() == token.as_bytes())
        .ok_or("The pairing code is invalid or expired")?;
    let stream_only = pairing[index].stream_only;
    let endpoint = remote_id
        .parse::<iroh::EndpointId>()
        .map_err(|_| "invalid mobile Iroh identity")?;
    let name = clean_device_name(name);
    let connection = open_connection(db_path)?;
    connection
            .execute(
                "INSERT INTO mobile_devices(endpoint_id,name,paired_at,last_seen,stream_only)
                 VALUES(?1,?2,?3,?3,?4)
                 ON CONFLICT(endpoint_id) DO UPDATE SET name=excluded.name,last_seen=excluded.last_seen,stream_only=excluded.stream_only",
                params![endpoint.to_string(), name, now.to_rfc3339(), stream_only],
            )
            .map_err(|error| error.to_string())?;
    pairing.remove(index);
    Ok(stream_only)
}
fn device_stream_only(db_path: &Path, remote_id: &str) -> Result<bool, String> {
    open_connection(db_path)?
        .query_row(
            "SELECT stream_only FROM mobile_devices WHERE endpoint_id=?1",
            [remote_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "This phone is not paired with Napstr".into())
}

fn check_request_permission(stream_only: bool, request: &ClientRequest) -> Result<(), String> {
    if !stream_only {
        return Ok(());
    }
    match request {
        ClientRequest::Library { .. }
        | ClientRequest::Search { .. }
        | ClientRequest::Audiobooks { .. }
        | ClientRequest::AudiobookLibrary { .. }
        | ClientRequest::Audiobook { .. }
        | ClientRequest::FetchAudio { .. }
        | ClientRequest::Available { .. }
        | ClientRequest::AlbumCovers { .. }
        // Seeing what the computer is playing is not a way of changing it.
        | ClientRequest::PlaybackState
        | ClientRequest::Status
        | ClientRequest::Ping => Ok(()),
        ClientRequest::Playback { .. } => Err(
            "This phone has read-only access, so it cannot control Napstr on the computer.".into(),
        ),
        ClientRequest::ReadOnlyTicket => Err(
            "This phone has read-only access, so it cannot lend access to another device.".into(),
        ),
        ClientRequest::ReportCover { .. } => Err(
            "This phone has read-only access, so it cannot publish reports.".into(),
        ),
        _ => Err("This phone has read-only access. Downloads on the Napstr host are not permitted.".into()),
    }
}

fn is_sha256_file_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A playback command with anything a phone should not be able to say removed.
///
/// A queue a phone sends is a list it built from what it could see, so entries
/// that are not file ids are dropped rather than refused: the queue still makes
/// sense without them, and refusing the whole request would be a worse answer
/// than playing the part of it that does. The track being asked for is not
/// dropped, because a request that cannot play its own track is a broken one.
fn bounded_playback(command: PlaybackCommand) -> Result<PlaybackCommand, String> {
    match command {
        PlaybackCommand::PlayTrack { file_id, queue } => {
            if !is_sha256_file_id(&file_id) {
                return Err("That is not a track this computer can look up".into());
            }
            if queue.len() > MAX_PLAY_QUEUE {
                return Err(format!(
                    "A queue of more than {MAX_PLAY_QUEUE} tracks is more than one request can carry"
                ));
            }
            Ok(PlaybackCommand::PlayTrack {
                file_id,
                queue: queue
                    .into_iter()
                    .filter(|candidate| is_sha256_file_id(candidate))
                    .collect(),
            })
        }
        other => Ok(other),
    }
}

/// Trim the host's bookkeeping (event ids, timestamps) from a resolved cover
/// before it crosses the wire.
fn remote_album_cover(cover: crate::network::AlbumCover) -> RemoteAlbumCover {
    RemoteAlbumCover {
        key: cover.key,
        art: cover.art,
        thumb: cover.thumb,
        mbid: cover.mbid,
        year: cover.year,
        genre: cover.genre,
        collection: cover.collection,
        source: cover.source,
        cover_file_id: cover.cover_file_id,
        mime: cover.mime,
        author: cover.author,
        seeder: cover.seeder,
    }
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
    let connection = open_connection(db_path)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS mobile_devices (
               endpoint_id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               paired_at TEXT NOT NULL,
               last_seen TEXT NOT NULL
             );",
        )
        .map_err(|error| error.to_string())?;
    let has_permission: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mobile_devices') WHERE name='stream_only')",
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if !has_permission {
        connection.execute_batch("ALTER TABLE mobile_devices ADD COLUMN stream_only INTEGER NOT NULL DEFAULT 0 CHECK(stream_only IN (0,1));")
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn load_devices(db_path: &Path) -> Result<Vec<PairedDevice>, String> {
    let connection = open_connection(db_path)?;
    let mut statement = connection
        .prepare(
            "SELECT endpoint_id,name,paired_at,last_seen,stream_only FROM mobile_devices ORDER BY last_seen DESC",
        )
        .map_err(|error| error.to_string())?;
    let devices = statement
        .query_map([], |row| {
            Ok(PairedDevice {
                endpoint_id: row.get(0)?,
                name: row.get(1)?,
                paired_at: row.get(2)?,
                last_seen: row.get(3)?,
                stream_only: row.get(4)?,
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

/// How many times the art this host would report has changed.
///
/// A phone caches covers, including "this host has none", so this is what tells
/// it that a cached answer may be stale rather than making it guess or ask
/// again on every render.
fn cover_revision(db_path: &Path) -> Result<u64, String> {
    let connection = open_connection(db_path)?;
    crate::cover::cover_revision(&connection)
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
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    write_bytes(send, &frame).await
}

async fn write_bytes(send: &mut iroh::endpoint::SendStream, bytes: &[u8]) -> Result<(), String> {
    let result = write_with_timeout(send, bytes, RESPONSE_WRITE_TIMEOUT).await;
    if result.is_err() {
        // A cancelled write may have sent part of a frame. Reset it instead of
        // appending an error frame or waiting again on the same blocked stream.
        let _ = send.reset(1u32.into());
    }
    result
}

async fn write_with_timeout<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    tokio::time::timeout(timeout, tokio::io::AsyncWriteExt::write_all(writer, bytes))
        .await
        .map_err(|_| "Napstrfy stopped reading the response".to_string())?
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_response_writes_release_request_slots() {
        let slots = Arc::new(tokio::sync::Semaphore::new(32));
        let mut readers = Vec::new();
        let mut requests = Vec::new();
        for _ in 0..32 {
            let permit = slots.clone().try_acquire_owned().unwrap();
            let (mut writer, reader) = tokio::io::duplex(1);
            readers.push(reader); // Connected peers deliberately never read.
            requests.push(tokio::spawn(async move {
                let _permit = permit;
                write_with_timeout(&mut writer, b"response", Duration::from_millis(50)).await
            }));
        }
        assert_eq!(slots.available_permits(), 0);
        tokio::time::timeout(Duration::from_secs(2), async {
            for request in requests {
                assert!(request.await.unwrap().unwrap_err().contains("stopped reading"));
            }
        }).await.unwrap();
        assert_eq!(slots.available_permits(), 32);
        drop(readers);
    }

    #[tokio::test]
    async fn response_writes_preserve_bytes_for_reading_clients() {
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let received = tokio::spawn(async move {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        write_with_timeout(&mut writer, b"complete response", Duration::from_secs(1)).await.unwrap();
        drop(writer);
        assert_eq!(received.await.unwrap(), b"complete response");
    }

    #[test]
    fn read_only_requests_can_cache_but_cannot_download_on_host_or_inspect_transfers() {
        for request in [
            ClientRequest::RequestDownload {
                file_id: "a".repeat(64),
                source_pubkeys: vec![],
                destination_folder: None,
            },
            ClientRequest::Transfers,
        ] {
            assert!(check_request_permission(true, &request).is_err());
            assert!(check_request_permission(false, &request).is_ok());
        }
        assert!(check_request_permission(
            true,
            &ClientRequest::FetchAudio { file_id: "a".repeat(64) }
        ).is_ok());
        assert!(check_request_permission(
            true,
            &ClientRequest::Library {
                query: String::new(),
                offset: 0,
                limit: 100
            }
        )
        .is_ok());
    }

    #[test]
    fn a_play_queue_is_bounded_and_filtered_to_file_ids() {
        let track = "a".repeat(64);
        let queued = "b".repeat(64);
        let PlaybackCommand::PlayTrack { file_id, queue } = bounded_playback(
            PlaybackCommand::PlayTrack {
                file_id: track.clone(),
                queue: vec![queued.clone(), "not-a-file".into(), String::new()],
            },
        )
        .unwrap()
        else {
            panic!("a play command must stay a play command")
        };
        assert_eq!(file_id, track);
        assert_eq!(queue, vec![queued]);

        // The track being asked for is never quietly swapped for another.
        assert!(bounded_playback(PlaybackCommand::PlayTrack {
            file_id: "nope".into(),
            queue: Vec::new(),
        })
        .is_err());
        assert!(bounded_playback(PlaybackCommand::PlayTrack {
            file_id: track,
            queue: vec!["c".repeat(64); MAX_PLAY_QUEUE + 1],
        })
        .is_err());

        // Anything that is not about a queue passes through untouched.
        assert_eq!(
            bounded_playback(PlaybackCommand::Next).unwrap(),
            PlaybackCommand::Next
        );
    }

    #[test]
    fn pairing_grants_are_separate_single_use_and_persisted() {
        let directory =
            std::env::temp_dir().join(format!("napstr-pairing-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let db = directory.join("napstr.sqlite3");
        let connection = open_connection(&db).unwrap();
        // An existing installation must retain its previous full access.
        connection.execute_batch("CREATE TABLE mobile_devices(endpoint_id TEXT PRIMARY KEY,name TEXT NOT NULL,paired_at TEXT NOT NULL,last_seen TEXT NOT NULL);
            INSERT INTO mobile_devices VALUES('legacy','Old phone','now','now');").unwrap();
        initialise_schema(&db).unwrap();
        initialise_schema(&db).unwrap();
        assert!(!device_stream_only(&db, "legacy").unwrap());
        let endpoint = SecretKey::generate().public().to_string();
        let mut sessions = vec![
            PairingSession {
                token: "full".into(),
                stream_only: false,
                expires_at: Utc::now().timestamp() + 60,
            },
            PairingSession {
                token: "stream".into(),
                stream_only: true,
                expires_at: Utc::now().timestamp() + 60,
            },
            PairingSession {
                token: "expired".into(),
                stream_only: true,
                expires_at: Utc::now().timestamp() - 1,
            },
        ];
        assert!(accept_pairing(&db, &mut sessions, &endpoint, "expired", "Guest").is_err());
        assert!(accept_pairing(&db, &mut sessions, &endpoint, "wrong", "Guest").is_err());
        // Older clients can also fetch/cache audio; the host enforces their read-only grant.
        assert!(accept_pairing(&db, &mut sessions, &endpoint, "stream", "Guest").unwrap());
        assert!(device_stream_only(&db, &endpoint).unwrap());
        assert!(accept_pairing(&db, &mut sessions, &endpoint, "stream", "Guest").is_err());
        assert!(!accept_pairing(&db, &mut sessions, &endpoint, "full", "Owner").unwrap());
        assert!(!device_stream_only(&db, &endpoint).unwrap());
        assert!(sessions.is_empty());
        sessions.push(PairingSession {
            token: "downgrade".into(),
            stream_only: true,
            expires_at: Utc::now().timestamp() + 60,
        });
        assert!(accept_pairing(&db, &mut sessions, &endpoint, "downgrade", "Guest").unwrap());
        assert!(
            load_devices(&db)
                .unwrap()
                .iter()
                .find(|device| device.endpoint_id == endpoint)
                .unwrap()
                .stream_only
        );
        connection
            .execute(
                "DELETE FROM mobile_devices WHERE endpoint_id=?1",
                [&endpoint],
            )
            .unwrap();
        assert!(device_stream_only(&db, &endpoint).is_err());
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

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
