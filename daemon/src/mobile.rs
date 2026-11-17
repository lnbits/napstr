//! Napstrfy's existing Iroh protocol, hosted without a desktop session.
use crate::{
    library::{Library, SharedFile},
    server::now,
};
use iroh::{endpoint::presets, Endpoint, SecretKey};
use napstr_remote_protocol::{
    ClientRequest, PairingTicket, RemoteAudiobook, RemoteAudiobookSummary, RemoteTrack,
    ServerResponse, ALPN, MAX_CONTROL_FRAME_BYTES, MAX_PAGE_SIZE, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{RwLock, Semaphore},
    task::JoinSet,
    time::timeout,
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub endpoint_id: String,
    pub name: String,
    pub paired_at: u64,
    pub stream_only: bool,
}

struct PairingSession {
    token: String,
    expires_at: u64,
    stream_only: bool,
}

struct Access {
    devices: BTreeMap<String, Device>,
    pending: Vec<PairingSession>,
    path: PathBuf,
}

impl Access {
    fn load(path: PathBuf) -> Result<Self, String> {
        let devices = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "Invalid saved Iroh devices")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self {
            devices,
            pending: Vec::new(),
            path,
        })
    }

    fn save(&self, devices: &BTreeMap<String, Device>) -> Result<(), String> {
        let mut temporary =
            tempfile::NamedTempFile::new_in(self.path.parent().ok_or("Missing state directory")?)
                .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temporary, devices).map_err(|e| e.to_string())?;
        temporary.as_file().sync_all().map_err(|e| e.to_string())?;
        temporary.persist(&self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn accept(&mut self, endpoint: &str, token: &str, name: &str) -> Result<bool, String> {
        self.pending.retain(|session| session.expires_at > now());
        let index = self
            .pending
            .iter()
            .position(|session| session.token == token)
            .ok_or("The pairing code is invalid or expired")?;
        let endpoint = endpoint
            .parse::<iroh::EndpointId>()
            .map_err(|_| "Invalid Iroh endpoint ID")?
            .to_string();
        let stream_only = self.pending[index].stream_only;
        let mut devices = self.devices.clone();
        devices.insert(
            endpoint.clone(),
            Device {
                endpoint_id: endpoint,
                name: clean_name(name),
                paired_at: now(),
                stream_only,
            },
        );
        self.save(&devices)?;
        self.devices = devices;
        self.pending.remove(index);
        Ok(stream_only)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingOffer {
    pub ticket: String,
    pub expires_at: u64,
    pub endpoint_id: String,
}

#[derive(Default)]
struct Catalogue {
    tracks: Vec<RemoteTrack>,
    books: Vec<RemoteAudiobook>,
}

pub struct MobileService {
    library: Arc<RwLock<Library>>,
    downloads: Arc<crate::downloads::Downloads>,
    root: PathBuf,
    data: PathBuf,
    access: Mutex<Access>,
    endpoint: RwLock<Option<Endpoint>>,
    start_lock: tokio::sync::Mutex<()>,
    revision: Arc<AtomicU64>,
    catalogue: RwLock<Option<(u64, Arc<Catalogue>)>>,
    connection_slots: Arc<Semaphore>,
    request_slots: Arc<Semaphore>,
    audio_slots: Arc<Semaphore>,
}

impl MobileService {
    pub fn new(
        library: Arc<RwLock<Library>>,
        root: PathBuf,
        data: PathBuf,
        downloads: Arc<crate::downloads::Downloads>,
    ) -> Result<Arc<Self>, String> {
        let revision = downloads.revision.clone();
        Ok(Arc::new(Self {
            library,
            downloads,
            root,
            access: Mutex::new(Access::load(data.join("iroh-devices.json"))?),
            data,
            endpoint: RwLock::new(None),
            start_lock: tokio::sync::Mutex::new(()),
            revision,
            catalogue: RwLock::new(None),
            connection_slots: Arc::new(Semaphore::new(16)),
            request_slots: Arc::new(Semaphore::new(32)),
            audio_slots: Arc::new(Semaphore::new(4)),
        }))
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub fn library_changed(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn devices(&self) -> Result<Vec<Device>, String> {
        Ok(self
            .access
            .lock()
            .map_err(|_| "Iroh access lock failed")?
            .devices
            .values()
            .cloned()
            .collect())
    }

    pub fn revoke(&self, endpoint_id: &str) -> Result<(), String> {
        let endpoint = endpoint_id
            .parse::<iroh::EndpointId>()
            .map_err(|_| "Invalid Iroh endpoint ID")?
            .to_string();
        let mut access = self.access.lock().map_err(|_| "Iroh access lock failed")?;
        let mut devices = access.devices.clone();
        if devices.remove(&endpoint).is_none() {
            return Err("That device is not paired".into());
        }
        access.save(&devices)?;
        access.devices = devices;
        Ok(())
    }

    fn authorise(&self, remote_id: &str) -> Result<bool, String> {
        self.access
            .lock()
            .map_err(|_| "Iroh access lock failed")?
            .devices
            .get(remote_id)
            .map(|device| device.stream_only)
            .ok_or_else(|| "This device is not paired with Napstr".into())
    }

    fn connection_allowed(&self, remote_id: &str) -> bool {
        let Ok(mut access) = self.access.lock() else {
            return false;
        };
        access.pending.retain(|session| session.expires_at > now());
        access.devices.contains_key(remote_id) || !access.pending.is_empty()
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), String> {
        let _guard = self.start_lock.lock().await;
        if self.endpoint.read().await.is_some() {
            return Ok(());
        }
        let key = load_key(&self.data.join("iroh-identity"))?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| format!("Iroh failed to start: {e}"))?;
        self.install_endpoint(endpoint).await;
        Ok(())
    }

    async fn install_endpoint(self: &Arc<Self>, endpoint: Endpoint) {
        *self.endpoint.write().await = Some(endpoint.clone());
        let service = self.clone();
        tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else { break; };
                        let Ok(permit) = service.connection_slots.clone().try_acquire_owned() else { incoming.refuse(); continue; };
                        let service = service.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            if let Ok(Ok(connection)) = timeout(Duration::from_secs(15), incoming).await {
                                service.handle_connection(connection).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
    }

    pub async fn stop(&self) {
        if let Some(endpoint) = self.endpoint.write().await.take() {
            endpoint.close().await;
        }
        if let Ok(mut access) = self.access.lock() {
            access.pending.clear();
        }
    }

    pub async fn pair(self: &Arc<Self>, stream_only: bool) -> Result<PairingOffer, String> {
        self.start().await?;
        let endpoint = self
            .endpoint
            .read()
            .await
            .clone()
            .ok_or("Iroh is not running")?;
        let _ = timeout(Duration::from_secs(12), endpoint.online()).await;
        self.pair_with_endpoint(&endpoint, stream_only)
    }

    fn pair_with_endpoint(
        &self,
        endpoint: &Endpoint,
        stream_only: bool,
    ) -> Result<PairingOffer, String> {
        let token = hex::encode(rand::random::<[u8; 32]>());
        let expires_at = now() + 300;
        let ticket = PairingTicket {
            version: PROTOCOL_VERSION,
            endpoint_id: endpoint.id().to_string(),
            endpoint_addr: serde_json::to_string(&endpoint.addr()).map_err(|e| e.to_string())?,
            token: token.clone(),
            expires_at: expires_at as i64,
            desktop_name: "Napstr daemon".into(),
        }
        .to_uri()?;
        let mut access = self.access.lock().map_err(|_| "Iroh access lock failed")?;
        // Match desktop: a new code replaces the previous code of that mode.
        access
            .pending
            .retain(|s| s.stream_only != stream_only && s.expires_at > now());
        access.pending.push(PairingSession {
            token,
            expires_at,
            stream_only,
        });
        Ok(PairingOffer {
            ticket,
            expires_at,
            endpoint_id: endpoint.id().to_string(),
        })
    }

    async fn handle_connection(self: Arc<Self>, connection: iroh::endpoint::Connection) {
        let remote_id = connection.remote_id().to_string();
        if !self.connection_allowed(&remote_id) {
            connection.close(1u32.into(), b"Not paired");
            return;
        }
        let mut requests = JoinSet::new();
        loop {
            if !self.connection_allowed(&remote_id) {
                break;
            }
            tokio::select! {
                streams = timeout(Duration::from_secs(300), connection.accept_bi()) => {
                    let Ok(Ok((mut send, mut receive))) = streams else { break; };
                    let Ok(permit) = self.request_slots.clone().try_acquire_owned() else {
                        let _ = send.reset(1u32.into()); let _ = receive.stop(1u32.into()); continue;
                    };
                    let service = self.clone();
                    let remote_id = remote_id.clone();
                    requests.spawn(async move {
                        let _permit = permit;
                        let request = timeout(Duration::from_secs(15), read_frame::<_, ClientRequest>(&mut receive)).await;
                        let result = match request {
                            Ok(Ok(request)) => service.serve(&remote_id, request, &mut send).await,
                            _ => Err("Invalid or timed-out Napstrfy request".into()),
                        };
                        if let Err(message) = result { let _ = write_response(&mut send, &ServerResponse::Error { message }).await; }
                        let _ = send.finish();
                    });
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
            }
        }
        connection.close(0u32.into(), b"Connection closed");
    }

    async fn catalogue(&self) -> Arc<Catalogue> {
        let revision = self.revision.load(Ordering::SeqCst);
        if let Some((cached_revision, catalogue)) = self.catalogue.read().await.as_ref() {
            if *cached_revision == revision {
                return catalogue.clone();
            }
        }
        let catalogue = Arc::new(build_catalogue(&self.root, &*self.library.read().await));
        *self.catalogue.write().await = Some((revision, catalogue.clone()));
        catalogue
    }

    async fn books(&self, query: &str, stream_only: bool) -> Result<Vec<RemoteAudiobook>, String> {
        let catalogue = self.catalogue().await;
        let mut books: Vec<_> = catalogue
            .books
            .iter()
            .filter(|book| matches_book(query, book))
            .cloned()
            .collect();
        if !stream_only {
            let remote = self.downloads.books(query, None).await?;
            let library = self.library.read().await;
            for mut book in remote {
                if books
                    .iter()
                    .any(|local| local.audiobook_id == book.audiobook_id)
                {
                    continue;
                }
                for chapter in &mut book.chapters {
                    if let Some(file) = library.get(&chapter.file_id) {
                        *chapter = remote_track(file);
                    }
                }
                books.push(book);
            }
        }
        books.sort_by(|a, b| a.title.cmp(&b.title));
        books.truncate(MAX_PAGE_SIZE);
        Ok(books)
    }

    async fn serve(
        &self,
        remote: &str,
        request: ClientRequest,
        send: &mut iroh::endpoint::SendStream,
    ) -> Result<(), String> {
        if let ClientRequest::Pair { token, device_name } = request {
            let stream_only = self
                .access
                .lock()
                .map_err(|_| "Iroh access lock failed")?
                .accept(remote, &token, &device_name)?;
            return write_response(
                send,
                &ServerResponse::Paired {
                    desktop_name: "Napstr daemon".into(),
                    stream_only,
                },
            )
            .await;
        }
        let stream_only = self.authorise(remote)?;
        check_permission(stream_only, &request)?;
        let response = match request {
            ClientRequest::Status => ServerResponse::Status {
                library_revision: self.revision.load(Ordering::SeqCst),
                stream_only,
            },
            ClientRequest::Ping => ServerResponse::Pong,
            ClientRequest::Library {
                query,
                offset,
                limit,
            } => {
                validate_query(&query)?;
                let catalogue = self.catalogue().await;
                let matching: Vec<_> = catalogue
                    .tracks
                    .iter()
                    .filter(|track| matches_query(&query, track))
                    .collect();
                ServerResponse::Library {
                    total: matching.len(),
                    tracks: matching
                        .into_iter()
                        .skip(offset)
                        .take(limit.clamp(1, MAX_PAGE_SIZE))
                        .cloned()
                        .collect(),
                }
            }
            ClientRequest::Search { query } => {
                validate_query(&query)?;
                if query.trim().is_empty() {
                    return Err("Search for between 1 and 120 characters".into());
                }
                let catalogue = self.catalogue().await;
                let mut tracks: Vec<_> = catalogue
                    .tracks
                    .iter()
                    .filter(|track| matches_query(&query, track))
                    .take(MAX_PAGE_SIZE)
                    .cloned()
                    .collect();
                if !stream_only {
                    let chapter_ids: std::collections::HashSet<_> = catalogue
                        .books
                        .iter()
                        .flat_map(|b| b.chapters.iter().map(|c| &c.file_id))
                        .collect();
                    for remote in self.downloads.search(&query).await? {
                        if !chapter_ids.contains(&remote.file_id)
                            && !tracks.iter().any(|t| t.file_id == remote.file_id)
                        {
                            tracks.push(remote);
                        }
                    }
                }
                tracks.truncate(MAX_PAGE_SIZE);
                ServerResponse::Search { tracks }
            }
            ClientRequest::Audiobooks { query } => {
                validate_query(&query)?;
                ServerResponse::Audiobooks {
                    audiobooks: self.books(&query, stream_only).await?,
                }
            }
            ClientRequest::AudiobookLibrary {
                query,
                offset,
                limit,
            } => {
                validate_query(&query)?;
                let books = self.books(&query, stream_only).await?;
                ServerResponse::AudiobookLibrary {
                    total: books.len(),
                    audiobooks: books
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
                        .collect(),
                }
            }
            ClientRequest::Audiobook { audiobook_id } => {
                let catalogue = self.catalogue().await;
                let book = if let Some(local) = catalogue
                    .books
                    .iter()
                    .find(|book| book.audiobook_id == audiobook_id)
                {
                    local.clone()
                } else {
                    if stream_only {
                        return Err("This audiobook is not in the local library".into());
                    }
                    let mut book = self
                        .downloads
                        .books("", Some(&audiobook_id))
                        .await?
                        .into_iter()
                        .next()
                        .ok_or("That audiobook is no longer available")?;
                    let library = self.library.read().await;
                    for chapter in &mut book.chapters {
                        if let Some(file) = library.get(&chapter.file_id) {
                            *chapter = remote_track(file);
                        }
                    }
                    book
                };
                ServerResponse::Audiobook { audiobook: book }
            }
            ClientRequest::Available { file_ids } => {
                if file_ids.len() > MAX_PAGE_SIZE || file_ids.iter().any(|id| !valid_id(id)) {
                    return Err("Invalid cached-file availability request".into());
                }
                let library = self.library.read().await;
                ServerResponse::Available {
                    file_ids: file_ids
                        .into_iter()
                        .filter(|id| {
                            library.get(id).is_some_and(|file| {
                                crate::library::checked_path(&self.root, &file.path).is_ok()
                            })
                        })
                        .collect(),
                }
            }
            ClientRequest::FetchAudio { file_id } => {
                let _slot = self
                    .audio_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "Audio streams are busy; try again shortly")?;
                let file = self
                    .library
                    .read()
                    .await
                    .get(&file_id)
                    .cloned()
                    .ok_or("This track is no longer shared")?;
                let root = self.root.clone();
                let data = self.data.clone();
                let source = file.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    crate::server::snapshot(&root, &data, &source)
                })
                .await
                .map_err(|e| e.to_string())??;
                self.authorise(remote)?;
                write_response(
                    send,
                    &ServerResponse::AudioReady {
                        track: remote_track(&file),
                    },
                )
                .await?;
                let mut audio = tokio::fs::File::from_std(snapshot);
                let mut bytes = vec![0; 256 * 1024];
                loop {
                    if self.authorise(remote).is_err() {
                        let _ = send.reset(1u32.into());
                        return Err("Device access was revoked".into());
                    }
                    let count = match audio.read(&mut bytes).await {
                        Ok(count) => count,
                        Err(error) => {
                            let _ = send.reset(1u32.into());
                            return Err(error.to_string());
                        }
                    };
                    if count == 0 {
                        return Ok(());
                    }
                    if let Err(error) = write_bytes(send, &bytes[..count]).await {
                        let _ = send.reset(1u32.into());
                        return Err(error);
                    }
                }
            }
            ClientRequest::RequestDownload {
                file_id,
                source_pubkeys,
                destination_folder,
            } => ServerResponse::DownloadRequested {
                request_id: self
                    .downloads
                    .request(file_id, source_pubkeys, destination_folder)
                    .await?,
            },
            ClientRequest::Transfers => ServerResponse::Transfers {
                transfers: self.downloads.transfers()?,
            },
            ClientRequest::Pair { .. } => unreachable!(),
        };
        write_response(send, &response).await
    }
}

fn load_key(path: &Path) -> Result<SecretKey, String> {
    match fs::read(path) {
        Ok(bytes) => {
            Ok(SecretKey::from_bytes(&bytes.try_into().map_err(|_| {
                "Saved Iroh identity has an invalid length"
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            let mut temporary =
                tempfile::NamedTempFile::new_in(path.parent().ok_or("Missing identity directory")?)
                    .map_err(|e| e.to_string())?;
            temporary
                .write_all(&key.to_bytes())
                .map_err(|e| e.to_string())?;
            temporary.as_file().sync_all().map_err(|e| e.to_string())?;
            temporary
                .persist_noclobber(path)
                .map_err(|e| e.to_string())?;
            Ok(key)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).take(64).collect();
    if cleaned.trim().is_empty() {
        "Napstrfy phone".into()
    } else {
        cleaned.trim().into()
    }
}

fn check_permission(stream_only: bool, request: &ClientRequest) -> Result<(), String> {
    if stream_only
        && matches!(
            request,
            ClientRequest::RequestDownload { .. } | ClientRequest::Transfers
        )
    {
        return Err(
            "This device has read-only access. Downloads on the Napstr host are not permitted."
                .into(),
        );
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn validate_query(query: &str) -> Result<(), String> {
    if query.chars().count() > 120 {
        Err("Searches are limited to 120 characters".into())
    } else {
        Ok(())
    }
}
pub(crate) fn matches_query(query: &str, track: &RemoteTrack) -> bool {
    let text = format!(
        "{} {} {} {} {}",
        track.filename, track.title, track.artist, track.album, track.tags
    )
    .to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}
pub(crate) fn matches_book(query: &str, book: &RemoteAudiobook) -> bool {
    let text = format!(
        "{} {} {} {}",
        book.title,
        book.author,
        book.narrator,
        book.chapters
            .iter()
            .map(|c| format!("{} {}", c.filename, c.title))
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}
fn remote_track(file: &SharedFile) -> RemoteTrack {
    RemoteTrack {
        file_id: file.file_id.clone(),
        filename: file.filename.clone(),
        title: file.title.clone(),
        artist: file.artist.clone(),
        album: file.album.clone(),
        format: file.format.clone(),
        mime: file.mime.clone(),
        size: file.size,
        tags: file.tags.into(),
        local: true,
        sources: Vec::new(),
    }
}

fn natural_key(value: &str) -> String {
    let mut key = String::new();
    let mut digits = String::new();
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_digit() {
            digits.push(character);
        } else {
            if !digits.is_empty() {
                key.push_str(&format!("#{:0>20}", digits.trim_start_matches('0')));
                digits.clear();
            }
            key.push(character);
        }
    }
    if !digits.is_empty() {
        key.push_str(&format!("#{:0>20}", digits.trim_start_matches('0')));
    }
    key
}

fn build_catalogue(root: &Path, library: &Library) -> Catalogue {
    let mut result = Catalogue::default();
    let mut books: BTreeMap<String, Vec<&SharedFile>> = BTreeMap::new();
    for file in library.values() {
        let relative = file.path.strip_prefix(root).unwrap_or(&file.path);
        let parts: Vec<_> = relative.components().collect();
        if parts
            .first()
            .is_some_and(|part| part.as_os_str() == "Audiobooks")
            && parts.len() >= 2
        {
            let group = if parts.len() == 2 {
                format!("@{}", file.file_id)
            } else {
                parts[1].as_os_str().to_string_lossy().into_owned()
            };
            books.entry(group).or_default().push(file);
        } else {
            result.tracks.push(remote_track(file));
        }
    }
    for (group, mut files) in books {
        if files.len() > 500 {
            result.tracks.extend(files.into_iter().map(remote_track));
            continue;
        }
        files.sort_by_cached_key(|file| {
            (
                if file.track_number > 0 { 0 } else { 1 },
                if file.track_number > 0 {
                    file.disc_number.max(1)
                } else {
                    0
                },
                file.track_number,
                natural_key(&file.path.to_string_lossy()),
                file.file_id.clone(),
            )
        });
        let title = if group.starts_with('@') {
            if files[0].title.is_empty() {
                Path::new(&files[0].filename)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            } else {
                files[0].title.clone()
            }
        } else {
            crate::audio::sanitise_public_text(&group.replace('_', " "))
        };
        let mut hash = Sha256::new();
        hash.update(b"napstr-audiobook-v1\0");
        for file in &files {
            hash.update(file.file_id.as_bytes());
        }
        result.books.push(RemoteAudiobook {
            audiobook_id: hex::encode(hash.finalize()),
            title,
            author: files
                .iter()
                .find(|f| !f.artist.is_empty())
                .map(|f| f.artist.clone())
                .unwrap_or_default(),
            narrator: String::new(),
            total_size: files.iter().map(|f| f.size).sum(),
            chapters: files.into_iter().map(remote_track).collect(),
        });
    }
    result.tracks.sort_by(|a, b| a.filename.cmp(&b.filename));
    result.books.sort_by(|a, b| a.title.cmp(&b.title));
    result
}

pub async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> Result<T, String> {
    let size = reader.read_u32().await.map_err(|e| e.to_string())? as usize;
    if size == 0 || size > MAX_CONTROL_FRAME_BYTES {
        return Err("Invalid Napstrfy frame size".into());
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid Napstrfy frame".into())
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), String> {
    let payload = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err("Napstrfy response is too large".into());
    }
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    timeout(WRITE_TIMEOUT, writer.write_all(&frame))
        .await
        .map_err(|_| "Napstrfy stopped reading the response")?
        .map_err(|e| e.to_string())
}

async fn write_response(
    send: &mut iroh::endpoint::SendStream,
    response: &ServerResponse,
) -> Result<(), String> {
    let result = write_frame(send, response).await;
    if result.is_err() {
        let _ = send.reset(1u32.into());
    }
    result
}

async fn write_bytes(send: &mut iroh::endpoint::SendStream, bytes: &[u8]) -> Result<(), String> {
    let result = timeout(WRITE_TIMEOUT, send.write_all(bytes))
        .await
        .map_err(|_| "Napstrfy stopped reading audio".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = send.reset(1u32.into());
    }
    result
}

#[cfg(test)]
#[path = "mobile_tests.rs"]
mod tests;
