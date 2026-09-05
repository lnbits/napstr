use futures_util::StreamExt;
use iroh::{endpoint::presets, Endpoint, EndpointAddr, EndpointId, SecretKey};
use napstr_remote_protocol::{
    ClientRequest, PairingTicket, RemoteAudiobook, RemoteAudiobookSummary, RemoteTrack,
    RemoteTransfer, ServerResponse, ALPN, MAX_CONTROL_FRAME_BYTES,
};
use quick_xml::{events::Event, Reader};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};
use tauri::{Manager, State};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Notify, RwLock},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedDesktop {
    endpoint_id: String,
    endpoint_addr: String,
    desktop_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompanionStatus {
    paired: bool,
    connected: bool,
    desktop_name: String,
    endpoint_id: String,
    library_revision: u64,
    error: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryPage {
    tracks: Vec<RemoteTrack>,
    total: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfflineLibrary {
    tracks: Vec<RemoteTrack>,
    total: usize,
    paired: bool,
    desktop_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedRemoteAudio {
    track: RemoteTrack,
    #[serde(default = "default_library_visible")]
    library_visible: bool,
}

fn default_library_visible() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AudiobookLibraryPage {
    audiobooks: Vec<RemoteAudiobookSummary>,
    total: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedAudio {
    url: String,
    track: RemoteTrack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodcastFeed {
    id: u64,
    title: String,
    author: String,
    description: String,
    feed_url: String,
    image: String,
    language: String,
    episode_count: u64,
    #[serde(default)]
    genres: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodcastEpisode {
    id: u64,
    feed_id: u64,
    feed_title: String,
    title: String,
    description: String,
    enclosure_url: String,
    enclosure_type: String,
    enclosure_length: u64,
    date_published: i64,
    duration: u64,
    image: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedPodcastAudio {
    url: String,
    downloaded: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PodcastDownload {
    episode: PodcastEpisode,
    progress: f64,
    status: String,
    ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPodcast {
    episode: PodcastEpisode,
    received: u64,
    total: u64,
    status: String,
    ready: bool,
}

#[derive(Deserialize)]
struct PublicPodcastSearch {
    results: Vec<PublicPodcastFeed>,
}

#[derive(Deserialize)]
struct PublicPodcastEpisodeLookup {
    results: Vec<PublicPodcastEpisode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicPodcastFeed {
    collection_id: Option<u64>,
    track_id: Option<u64>,
    collection_name: Option<String>,
    track_name: Option<String>,
    artist_name: Option<String>,
    feed_url: Option<String>,
    artwork_url600: Option<String>,
    artwork_url100: Option<String>,
    country: Option<String>,
    track_count: Option<u64>,
    genres: Option<Vec<String>>,
    primary_genre_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicPodcastEpisode {
    track_id: Option<u64>,
    collection_id: Option<u64>,
    collection_name: Option<String>,
    track_name: Option<String>,
    description: Option<String>,
    short_description: Option<String>,
    episode_url: Option<String>,
    episode_content_type: Option<String>,
    episode_file_extension: Option<String>,
    track_time_millis: Option<u64>,
    release_date: Option<String>,
    artwork_url600: Option<String>,
    artwork_url160: Option<String>,
}

#[derive(Default)]
struct PodcastEpisodeBuilder {
    guid: String,
    title: String,
    description: String,
    enclosure_url: String,
    enclosure_type: String,
    enclosure_length: u64,
    date_published: i64,
    duration: u64,
    image: String,
}

struct MediaEntry {
    track: RemoteTrack,
    final_path: PathBuf,
    temporary_path: PathBuf,
    received: AtomicU64,
    complete: AtomicBool,
    error: StdMutex<Option<String>>,
    changed: Notify,
}

impl MediaEntry {
    fn completed(track: RemoteTrack, final_path: PathBuf, temporary_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            received: AtomicU64::new(track.size),
            complete: AtomicBool::new(true),
            track,
            final_path,
            temporary_path,
            error: StdMutex::new(None),
            changed: Notify::new(),
        })
    }

    fn downloading(track: RemoteTrack, final_path: PathBuf, temporary_path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            track,
            final_path,
            temporary_path,
            received: AtomicU64::new(0),
            complete: AtomicBool::new(false),
            error: StdMutex::new(None),
            changed: Notify::new(),
        })
    }

    fn failure(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }

    fn fail(&self, message: String) {
        if let Ok(mut error) = self.error.lock() {
            *error = Some(message);
        }
        self.changed.notify_waiters();
    }
}

struct MediaServer {
    port: u16,
    token: String,
    entries: RwLock<HashMap<String, Arc<MediaEntry>>>,
    prepare_lock: Mutex<()>,
    scheduled_prefetches: Mutex<HashSet<(String, String)>>,
}

impl MediaServer {
    fn start() -> Result<Arc<Self>, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("could not start the private audio player: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("could not configure the private audio player: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let token = hex::encode(SecretKey::generate().to_bytes());
        let server = Arc::new(Self {
            port,
            token,
            entries: RwLock::new(HashMap::new()),
            prepare_lock: Mutex::new(()),
            scheduled_prefetches: Mutex::new(HashSet::new()),
        });
        let service = server.clone();
        tauri::async_runtime::spawn(async move {
            let Ok(listener) = TcpListener::from_std(listener) else {
                return;
            };
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let service = service.clone();
                tokio::spawn(async move {
                    let _ = service.serve(socket).await;
                });
            }
        });
        Ok(server)
    }

    fn url(&self, track: &RemoteTrack) -> Result<String, String> {
        let extension = safe_extension(&track.format)?;
        Ok(format!(
            "http://127.0.0.1:{}/{}/{}.{}",
            self.port, self.token, track.file_id, extension
        ))
    }

    async fn entry(&self, file_id: &str) -> Option<Arc<MediaEntry>> {
        self.entries.read().await.get(file_id).cloned()
    }

    async fn insert(&self, entry: Arc<MediaEntry>) {
        self.entries
            .write()
            .await
            .insert(entry.track.file_id.clone(), entry);
    }

    async fn wait_until_complete(&self, file_id: &str) -> Result<(), String> {
        let entry = self
            .entry(file_id)
            .await
            .ok_or("the current track is not being cached")?;
        loop {
            let changed = entry.changed.notified();
            if let Some(error) = entry.failure() {
                return Err(error);
            }
            if entry.complete.load(Ordering::Acquire) {
                return Ok(());
            }
            changed.await;
        }
    }

    async fn serve(self: Arc<Self>, mut socket: TcpStream) -> Result<(), String> {
        let mut request = Vec::with_capacity(2048);
        let mut buffer = [0u8; 2048];
        loop {
            let count = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buffer))
                .await
                .map_err(|_| "local audio request timed out".to_string())?
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Ok(());
            }
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if request.len() > 16 * 1024 {
                return write_http_error(&mut socket, 431, "Request Header Fields Too Large").await;
            }
        }
        let request = std::str::from_utf8(&request).map_err(|_| "invalid HTTP request")?;
        let mut lines = request.split("\r\n");
        let first = lines.next().ok_or("empty HTTP request")?;
        let mut first = first.split_whitespace();
        let method = first.next().ok_or("missing HTTP method")?;
        let path = first.next().ok_or("missing HTTP path")?;
        if method != "GET" && method != "HEAD" {
            return write_http_error(&mut socket, 405, "Method Not Allowed").await;
        }
        let mut segments = path.trim_start_matches('/').split('/');
        if segments.next() != Some(self.token.as_str()) {
            return write_http_error(&mut socket, 404, "Not Found").await;
        }
        let requested = segments.next().ok_or("missing audio ID")?;
        if segments.next().is_some() {
            return write_http_error(&mut socket, 404, "Not Found").await;
        }
        let (file_id, extension) = requested.rsplit_once('.').ok_or("invalid audio ID")?;
        validate_file_id(file_id)?;
        let entry = self.entry(file_id).await.ok_or("audio is not prepared")?;
        if safe_extension(&entry.track.format)? != extension {
            return write_http_error(&mut socket, 404, "Not Found").await;
        }
        let range_header = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range")
                .then(|| value.trim().to_string())
        });
        let range = match requested_audio_range(range_header.as_deref(), entry.track.size) {
            Ok(range) => range,
            Err(()) => {
                let response = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n",
                    entry.track.size
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
        };
        let (start, end, status) = match range {
            Some((start, end)) => (start, end, "206 Partial Content"),
            None => (0, entry.track.size - 1, "200 OK"),
        };
        let mut headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: private, no-store\r\nConnection: close\r\n",
            audio_mime(extension).ok_or("unsupported audio format")?,
            end - start + 1
        );
        if range.is_some() {
            headers.push_str(&format!(
                "Content-Range: bytes {start}-{end}/{}\r\n",
                entry.track.size
            ));
        }
        headers.push_str("\r\n");
        socket
            .write_all(headers.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        if method == "HEAD" {
            return Ok(());
        }
        stream_cached_audio(&mut socket, entry, start, end).await
    }
}

async fn write_http_error(socket: &mut TcpStream, code: u16, reason: &str) -> Result<(), String> {
    let response =
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    socket
        .write_all(response.as_bytes())
        .await
        .map_err(|error| error.to_string())
}

async fn stream_cached_audio(
    socket: &mut TcpStream,
    entry: Arc<MediaEntry>,
    start: u64,
    end: u64,
) -> Result<(), String> {
    let mut offset = start;
    let mut file = loop {
        let changed = entry.changed.notified();
        let path = if entry.complete.load(Ordering::Acquire) {
            &entry.final_path
        } else {
            &entry.temporary_path
        };
        match tokio::fs::File::open(path).await {
            Ok(file) => break file,
            Err(_) => {
                if let Some(error) = entry.failure() {
                    return Err(error);
                }
                changed.await;
            }
        }
    };
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| error.to_string())?;
    let mut buffer = vec![0u8; 128 * 1024];
    while offset <= end {
        let changed = entry.changed.notified();
        if let Some(error) = entry.failure() {
            return Err(error);
        }
        let available = entry.received.load(Ordering::Acquire);
        if available <= offset {
            if entry.complete.load(Ordering::Acquire) {
                return Err("verified audio cache ended unexpectedly".into());
            }
            changed.await;
            continue;
        }
        let count = (available - offset)
            .min(end - offset + 1)
            .min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])
            .await
            .map_err(|error| format!("could not read the audio cache: {error}"))?;
        socket
            .write_all(&buffer[..count])
            .await
            .map_err(|error| error.to_string())?;
        offset += count as u64;
    }
    Ok(())
}

struct PodcastStore {
    root: PathBuf,
    metadata_client: reqwest::Client,
    download_client: reqwest::Client,
    downloads: RwLock<HashMap<u64, StoredPodcast>>,
}

impl PodcastStore {
    fn new(app_data: &Path) -> Result<Arc<Self>, String> {
        let root = app_data.join("podcasts");
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let mut downloads = HashMap::new();
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some(stored) = fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<StoredPodcast>(&bytes).ok())
                else {
                    continue;
                };
                if stored.ready
                    && validate_podcast_episode(&stored.episode).is_ok()
                    && podcast_audio_path(&root, &stored.episode).is_ok_and(|audio| audio.is_file())
                {
                    downloads.insert(stored.episode.id, stored);
                }
            }
        }
        let metadata_client =
            podcast_http_client(Duration::from_secs(20), Duration::from_secs(10))?;
        let download_client =
            podcast_http_client(Duration::from_secs(60 * 60), Duration::from_secs(45))?;
        Ok(Arc::new(Self {
            root,
            metadata_client,
            download_client,
            downloads: RwLock::new(downloads),
        }))
    }

    async fn feed_episodes(
        &self,
        feed: &PodcastFeed,
        limit: usize,
    ) -> Result<Vec<PodcastEpisode>, String> {
        // Older directory entries may not expose episodes through lookup. RSS is
        // retained as a native compatibility fallback. Apple directory requests
        // use Android WebView networking and are parsed by a separate command.
        let url = reqwest::Url::parse(&feed.feed_url)
            .map_err(|_| "Podcast directory returned an invalid feed URL")?;
        if !safe_public_https_url(&url) {
            return Err("Only public HTTPS podcast feeds are supported".into());
        }
        let bytes = self
            .get_bounded(url, 8 * 1024 * 1024, "podcast publisher")
            .await?;
        parse_podcast_feed(feed, &bytes, limit.clamp(1, 50))
    }

    async fn get_bounded(
        &self,
        url: reqwest::Url,
        maximum: u64,
        source: &str,
    ) -> Result<Vec<u8>, String> {
        let request = async {
            let response = self
                .metadata_client
                .get(url)
                .send()
                .await
                .map_err(|error| format!("Could not reach {source}: {error}"))?;
            if !response.status().is_success() || !safe_public_https_url(response.url()) {
                return Err(format!("{source} returned {}", response.status()));
            }
            if response
                .content_length()
                .is_some_and(|length| length > maximum)
            {
                return Err(format!("{source} response is too large"));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| format!("Could not read {source}: {error}"))?;
                if bytes.len().saturating_add(chunk.len()) > maximum as usize {
                    return Err(format!("{source} response is too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        tokio::time::timeout(Duration::from_secs(15), request)
            .await
            .map_err(|_| {
                format!(
                    "{source} did not respond within 15 seconds. Check your connection and retry."
                )
            })?
    }

    async fn list(&self) -> Vec<PodcastDownload> {
        let mut downloads = self
            .downloads
            .read()
            .await
            .values()
            .map(|stored| PodcastDownload {
                episode: stored.episode.clone(),
                progress: if stored.total == 0 {
                    0.0
                } else {
                    (stored.received as f64 / stored.total as f64 * 100.0).clamp(0.0, 100.0)
                },
                status: stored.status.clone(),
                ready: stored.ready,
            })
            .collect::<Vec<_>>();
        downloads.sort_by(|left, right| {
            right
                .episode
                .date_published
                .cmp(&left.episode.date_published)
        });
        downloads
    }

    async fn start(self: &Arc<Self>, episode: PodcastEpisode) -> Result<(), String> {
        validate_podcast_episode(&episode)?;
        {
            let mut downloads = self.downloads.write().await;
            if downloads
                .get(&episode.id)
                .is_some_and(|download| download.ready || download.status == "Downloading")
            {
                return Ok(());
            }
            downloads.insert(
                episode.id,
                StoredPodcast {
                    total: episode.enclosure_length,
                    episode: episode.clone(),
                    received: 0,
                    status: "Downloading".into(),
                    ready: false,
                },
            );
        }
        let store = self.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = store.download(episode.clone()).await {
                let _ = tokio::fs::remove_file(store.temporary_path(episode.id)).await;
                if let Some(download) = store.downloads.write().await.get_mut(&episode.id) {
                    download.status = format!("Failed: {error}");
                    download.ready = false;
                }
            }
        });
        Ok(())
    }

    async fn download(&self, episode: PodcastEpisode) -> Result<(), String> {
        let response = self
            .download_client
            .get(&episode.enclosure_url)
            .send()
            .await
            .map_err(|error| format!("Podcast download failed: {error}"))?;
        if !response.status().is_success() || !safe_public_https_url(response.url()) {
            return Err(format!("publisher returned {}", response.status()));
        }
        let advertised = episode.enclosure_length;
        let response_length = response.content_length().unwrap_or_default();
        let total = response_length.max(advertised);
        if total > 2 * 1024 * 1024 * 1024 {
            return Err("podcast episode is larger than 2 GB".into());
        }
        if let Some(download) = self.downloads.write().await.get_mut(&episode.id) {
            download.total = total;
        }
        let temporary = self.temporary_path(episode.id);
        let final_path = podcast_audio_path(&self.root, &episode)?;
        let _ = tokio::fs::remove_file(&temporary).await;
        let mut output = tokio::fs::File::create(&temporary)
            .await
            .map_err(|error| error.to_string())?;
        let mut stream = response.bytes_stream();
        let mut received = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("publisher stream failed: {error}"))?;
            received = received.saturating_add(chunk.len() as u64);
            if received > 2 * 1024 * 1024 * 1024
                || (response_length > 0 && received > response_length)
            {
                return Err("publisher sent more audio than advertised".into());
            }
            output
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(download) = self.downloads.write().await.get_mut(&episode.id) {
                download.received = received;
            }
        }
        output.flush().await.map_err(|error| error.to_string())?;
        drop(output);
        if received == 0 || (response_length > 0 && received != response_length) {
            return Err("podcast download ended before the advertised size".into());
        }
        tokio::fs::rename(&temporary, &final_path)
            .await
            .map_err(|error| error.to_string())?;
        let stored = {
            let mut downloads = self.downloads.write().await;
            let stored = downloads
                .get_mut(&episode.id)
                .ok_or("podcast download disappeared")?;
            stored.received = received;
            stored.total = received;
            stored.status = "Downloaded".into();
            stored.ready = true;
            stored.clone()
        };
        tokio::fs::write(
            self.metadata_path(episode.id),
            serde_json::to_vec(&stored).map_err(|error| error.to_string())?,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn playback_url(
        &self,
        episode: PodcastEpisode,
        media: Arc<MediaServer>,
    ) -> Result<CachedPodcastAudio, String> {
        validate_podcast_episode(&episode)?;
        let stored = self.downloads.read().await.get(&episode.id).cloned();
        let Some(stored) = stored.filter(|stored| stored.ready) else {
            return Ok(CachedPodcastAudio {
                url: episode.enclosure_url,
                downloaded: false,
            });
        };
        let path = podcast_audio_path(&self.root, &stored.episode)?;
        let size = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .len();
        if size == 0 || size != stored.received {
            return Err("the offline podcast copy is incomplete".into());
        }
        let track = podcast_media_track(&stored.episode, size)?;
        media
            .insert(MediaEntry::completed(
                track.clone(),
                path,
                self.temporary_path(stored.episode.id),
            ))
            .await;
        Ok(CachedPodcastAudio {
            url: media.url(&track)?,
            downloaded: true,
        })
    }

    fn metadata_path(&self, episode_id: u64) -> PathBuf {
        self.root.join(format!("{episode_id}.json"))
    }

    fn temporary_path(&self, episode_id: u64) -> PathBuf {
        self.root.join(format!(".{episode_id}.part"))
    }
}

fn podcast_http_client(
    request_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // Iroh enables Reqwest's Hickory resolver for its own networking. Cargo
        // features are additive, so that would otherwise also make podcast
        // requests use Hickory's Android/JNI system-configuration path. That
        // path can block before Reqwest's request timeout starts. The platform
        // resolver is the reliable choice for ordinary public HTTPS feeds.
        .no_hickory_dns()
        .connect_timeout(Duration::from_secs(8))
        .read_timeout(read_timeout)
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !safe_public_https_url(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .user_agent("Napstrfy/0.1 (https://napstr.net)")
        .build()
        .map_err(|error| error.to_string())
}

fn public_podcast_feed(feed: PublicPodcastFeed) -> Option<PodcastFeed> {
    let id = feed.collection_id.or(feed.track_id)?;
    let feed_url = normalized_public_https_url(feed.feed_url.as_deref()?, None)?;
    if id == 0 {
        return None;
    }
    let image = feed
        .artwork_url600
        .or(feed.artwork_url100)
        .and_then(|value| normalized_public_https_url(&value, None))
        .map(|url| url.to_string())
        .unwrap_or_default();
    let mut genres = feed
        .genres
        .unwrap_or_default()
        .into_iter()
        .map(|genre| clean_podcast_text(&genre, 64))
        .filter(|genre| !genre.is_empty())
        .collect::<Vec<_>>();
    if let Some(primary) = feed.primary_genre_name {
        let primary = clean_podcast_text(&primary, 64);
        if !primary.is_empty()
            && !genres
                .iter()
                .any(|genre| genre.eq_ignore_ascii_case(&primary))
        {
            genres.insert(0, primary);
        }
    }
    let mut unique_genres = Vec::new();
    for genre in genres {
        if !unique_genres
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&genre))
        {
            unique_genres.push(genre);
        }
        if unique_genres.len() == 12 {
            break;
        }
    }
    Some(PodcastFeed {
        id,
        title: clean_podcast_text(
            feed.collection_name
                .or(feed.track_name)
                .as_deref()
                .unwrap_or("Untitled podcast"),
            300,
        ),
        author: clean_podcast_text(feed.artist_name.as_deref().unwrap_or_default(), 200),
        description: String::new(),
        feed_url: feed_url.to_string(),
        image,
        language: clean_podcast_text(feed.country.as_deref().unwrap_or_default(), 32),
        episode_count: feed.track_count.unwrap_or_default(),
        genres: unique_genres,
    })
}

fn parse_public_podcast_search(payload: &str, limit: usize) -> Result<Vec<PodcastFeed>, String> {
    if payload.len() > 4 * 1024 * 1024 {
        return Err("Podcast directory response is too large".into());
    }
    let response: PublicPodcastSearch = serde_json::from_str(payload)
        .map_err(|_| "Podcast directory returned invalid search results".to_string())?;
    Ok(response
        .results
        .into_iter()
        .take(limit.clamp(1, 50))
        .filter_map(public_podcast_feed)
        .collect())
}

fn parse_public_podcast_episodes(
    feed: &PodcastFeed,
    payload: &str,
    limit: usize,
) -> Result<Vec<PodcastEpisode>, String> {
    if payload.len() > 8 * 1024 * 1024 {
        return Err("Podcast directory response is too large".into());
    }
    let response: PublicPodcastEpisodeLookup = serde_json::from_str(payload)
        .map_err(|_| "Podcast directory returned invalid episode results".to_string())?;
    Ok(response
        .results
        .into_iter()
        .filter_map(|episode| public_podcast_episode(feed, episode))
        .take(limit.clamp(1, 50))
        .collect())
}

fn public_podcast_episode(
    feed: &PodcastFeed,
    episode: PublicPodcastEpisode,
) -> Option<PodcastEpisode> {
    let enclosure = normalized_public_https_url(episode.episode_url.as_deref()?, None)?;
    let extension = episode
        .episode_file_extension
        .as_deref()
        .unwrap_or_default()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let enclosure_type = match extension.as_str() {
        "mp3" => "audio/mpeg".to_string(),
        "m4a" | "mp4" | "aac" => "audio/mp4".to_string(),
        "ogg" | "oga" => "audio/ogg".to_string(),
        "opus" => "audio/opus".to_string(),
        "wav" => "audio/wav".to_string(),
        _ if episode
            .episode_content_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("audio")) =>
        {
            podcast_mime_from_url(&enclosure)?
        }
        _ => podcast_mime_from_url(&enclosure)?,
    };
    let id = episode.track_id.unwrap_or_else(|| {
        let digest = Sha256::digest(enclosure.as_str().as_bytes());
        u64::from_be_bytes(digest[..8].try_into().unwrap_or_default()).max(1)
    });
    let image = episode
        .artwork_url600
        .or(episode.artwork_url160)
        .and_then(|value| normalized_public_https_url(&value, None))
        .map(|url| url.to_string())
        .unwrap_or_else(|| feed.image.clone());
    let description = episode.description.or(episode.short_description);
    Some(PodcastEpisode {
        id,
        feed_id: episode.collection_id.unwrap_or(feed.id),
        feed_title: clean_podcast_text(
            episode.collection_name.as_deref().unwrap_or(&feed.title),
            300,
        ),
        title: clean_podcast_text(
            episode.track_name.as_deref().unwrap_or("Untitled episode"),
            500,
        ),
        description: clean_podcast_text(description.as_deref().unwrap_or_default(), 4_000),
        enclosure_url: enclosure.to_string(),
        enclosure_type,
        enclosure_length: 0,
        date_published: episode
            .release_date
            .as_deref()
            .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
            .map(|date| date.timestamp())
            .unwrap_or_default(),
        duration: episode.track_time_millis.unwrap_or_default() / 1_000,
        image,
    })
}

fn parse_podcast_feed(
    feed: &PodcastFeed,
    bytes: &[u8],
    limit: usize,
) -> Result<Vec<PodcastEpisode>, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut current_tag = String::new();
    let mut current: Option<PodcastEpisodeBuilder> = None;
    let mut episodes = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let tag =
                    String::from_utf8_lossy(element.local_name().as_ref()).to_ascii_lowercase();
                if tag == "item" || tag == "entry" {
                    current = Some(PodcastEpisodeBuilder::default());
                } else if let Some(episode) = current.as_mut() {
                    apply_podcast_element_attributes(&reader, &element, &tag, episode);
                }
                current_tag = tag;
            }
            Ok(Event::Empty(element)) => {
                if let Some(episode) = current.as_mut() {
                    let tag =
                        String::from_utf8_lossy(element.local_name().as_ref()).to_ascii_lowercase();
                    apply_podcast_element_attributes(&reader, &element, &tag, episode);
                }
            }
            Ok(Event::Text(text)) => {
                if let (Some(episode), Ok(value)) = (current.as_mut(), text.decode()) {
                    apply_podcast_text(episode, &current_tag, &value);
                }
            }
            Ok(Event::CData(text)) => {
                if let (Some(episode), Ok(value)) = (current.as_mut(), text.decode()) {
                    apply_podcast_text(episode, &current_tag, &value);
                }
            }
            Ok(Event::End(element)) => {
                let tag =
                    String::from_utf8_lossy(element.local_name().as_ref()).to_ascii_lowercase();
                if tag == "item" || tag == "entry" {
                    if let Some(episode) = current
                        .take()
                        .and_then(|item| finish_podcast_episode(feed, item))
                    {
                        episodes.push(episode);
                        if episodes.len() >= limit {
                            break;
                        }
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err("The publisher returned an invalid podcast feed".into()),
            _ => {}
        }
        buffer.clear();
    }
    Ok(episodes)
}

fn apply_podcast_element_attributes(
    reader: &Reader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    tag: &str,
    episode: &mut PodcastEpisodeBuilder,
) {
    let mut url = None;
    let mut mime = None;
    let mut length = None;
    let mut relationship = None;
    for attribute in element.attributes().with_checks(false).flatten() {
        let key = String::from_utf8_lossy(attribute.key.local_name().as_ref()).to_ascii_lowercase();
        let Ok(value) = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
        else {
            continue;
        };
        match key.as_str() {
            "url" | "href" => url = Some(value.into_owned()),
            "type" => mime = Some(value.into_owned()),
            "length" => length = value.parse().ok(),
            "rel" => relationship = Some(value.into_owned()),
            _ => {}
        }
    }
    if tag == "image" {
        if let Some(value) = url {
            episode.image = value;
        }
        return;
    }
    let relationship = relationship.as_deref().unwrap_or_default();
    let media_content = tag == "content" || tag == "source";
    if tag == "enclosure"
        || (tag == "link" && relationship.eq_ignore_ascii_case("enclosure"))
        || media_content
    {
        if let Some(value) = url {
            episode.enclosure_url = value;
        }
        if let Some(value) = mime {
            episode.enclosure_type = value;
        }
        if let Some(value) = length {
            episode.enclosure_length = value;
        }
    }
}

fn apply_podcast_text(episode: &mut PodcastEpisodeBuilder, tag: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match tag {
        "guid" | "id" => episode.guid.push_str(value),
        "title" => episode.title.push_str(value),
        "description" | "summary" | "content" => episode.description.push_str(value),
        "pubdate" | "published" | "updated" => {
            episode.date_published = chrono::DateTime::parse_from_rfc2822(value)
                .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
                .map(|date| date.timestamp())
                .unwrap_or_default();
        }
        "duration" => episode.duration = parse_podcast_duration(value),
        _ => {}
    }
}

fn finish_podcast_episode(
    feed: &PodcastFeed,
    item: PodcastEpisodeBuilder,
) -> Option<PodcastEpisode> {
    let feed_url = normalized_public_https_url(&feed.feed_url, None)?;
    let enclosure = normalized_public_https_url(&item.enclosure_url, Some(&feed_url))?;
    let enclosure_type = if item
        .enclosure_type
        .to_ascii_lowercase()
        .starts_with("audio/")
    {
        clean_podcast_text(&item.enclosure_type, 100)
    } else {
        podcast_mime_from_url(&enclosure)?
    };
    let identity = if item.guid.is_empty() {
        enclosure.as_str()
    } else {
        &item.guid
    };
    let digest = Sha256::digest(identity.as_bytes());
    let id = u64::from_be_bytes(digest[..8].try_into().ok()?).max(1);
    let image = normalized_public_https_url(&item.image, Some(&feed_url))
        .map(|url| url.to_string())
        .unwrap_or_else(|| feed.image.clone());
    Some(PodcastEpisode {
        id,
        feed_id: feed.id,
        feed_title: feed.title.clone(),
        title: clean_podcast_text(
            if item.title.is_empty() {
                "Untitled episode"
            } else {
                &item.title
            },
            500,
        ),
        description: clean_podcast_text(&item.description, 4_000),
        enclosure_url: enclosure.to_string(),
        enclosure_type,
        enclosure_length: item.enclosure_length,
        date_published: item.date_published,
        duration: item.duration,
        image,
    })
}

fn podcast_mime_from_url(url: &reqwest::Url) -> Option<String> {
    let path = url.path().to_ascii_lowercase();
    if path.ends_with(".mp3") {
        Some("audio/mpeg".into())
    } else if path.ends_with(".m4a") || path.ends_with(".mp4") {
        Some("audio/mp4".into())
    } else if path.ends_with(".ogg") || path.ends_with(".oga") {
        Some("audio/ogg".into())
    } else if path.ends_with(".opus") {
        Some("audio/opus".into())
    } else if path.ends_with(".wav") {
        Some("audio/wav".into())
    } else {
        None
    }
}

fn parse_podcast_duration(value: &str) -> u64 {
    value
        .split(':')
        .filter_map(|part| part.trim().parse::<u64>().ok())
        .fold(0u64, |total, part| {
            total.saturating_mul(60).saturating_add(part)
        })
}

fn clean_podcast_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
        })
        .take(limit)
        .collect::<String>()
        .trim()
        .to_string()
}

fn validate_podcast_episode(episode: &PodcastEpisode) -> Result<(), String> {
    if episode.id == 0
        || episode.feed_id == 0
        || episode.enclosure_length > 2 * 1024 * 1024 * 1024
        || !episode
            .enclosure_type
            .to_ascii_lowercase()
            .starts_with("audio/")
    {
        return Err("Podcast directory returned an invalid audio episode".into());
    }
    let url = reqwest::Url::parse(&episode.enclosure_url)
        .map_err(|_| "Podcast directory returned an invalid enclosure URL")?;
    if !safe_public_https_url(&url) {
        return Err("Only public HTTPS podcast audio is supported".into());
    }
    podcast_extension(episode)?;
    Ok(())
}

fn safe_public_https_url(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return false;
    }
    host.parse::<std::net::IpAddr>()
        .map(|address| match address {
            std::net::IpAddr::V4(address) => {
                !(address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_unspecified())
            }
            std::net::IpAddr::V6(address) => {
                !(address.is_loopback() || address.is_unspecified() || address.is_unique_local())
            }
        })
        .unwrap_or(true)
}

fn normalized_public_https_url(value: &str, base: Option<&reqwest::Url>) -> Option<reqwest::Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut url = reqwest::Url::parse(value)
        .ok()
        .or_else(|| base.and_then(|base| base.join(value).ok()))?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    safe_public_https_url(&url).then_some(url)
}

fn podcast_extension(episode: &PodcastEpisode) -> Result<&'static str, String> {
    let mime = episode.enclosure_type.to_ascii_lowercase();
    if mime.contains("mpeg") || mime.contains("mp3") {
        return Ok("mp3");
    }
    if mime.contains("mp4") || mime.contains("m4a") || mime.contains("aac") {
        return Ok("m4a");
    }
    if mime.contains("ogg") {
        return Ok("ogg");
    }
    if mime.contains("opus") {
        return Ok("opus");
    }
    if mime.contains("wav") {
        return Ok("wav");
    }
    Err("This podcast audio format is not supported".into())
}

fn podcast_audio_path(root: &Path, episode: &PodcastEpisode) -> Result<PathBuf, String> {
    Ok(root.join(format!("{}.{}", episode.id, podcast_extension(episode)?)))
}

fn podcast_media_track(episode: &PodcastEpisode, size: u64) -> Result<RemoteTrack, String> {
    let extension = podcast_extension(episode)?;
    let file_id = hex::encode(Sha256::digest(episode.enclosure_url.as_bytes()));
    Ok(RemoteTrack {
        file_id,
        filename: format!("{}.{}", episode.id, extension),
        title: episode.title.clone(),
        artist: episode.feed_title.clone(),
        album: episode.feed_title.clone(),
        format: extension.to_ascii_uppercase(),
        mime: episode.enclosure_type.clone(),
        size,
        tags: "podcast".into(),
        local: true,
        sources: Vec::new(),
    })
}

struct RemoteClient {
    app_data: PathBuf,
    endpoint: tokio::sync::RwLock<Option<Endpoint>>,
    connection: tokio::sync::RwLock<Option<iroh::endpoint::Connection>>,
    desktop: tokio::sync::RwLock<Option<SavedDesktop>>,
    start_lock: tokio::sync::Mutex<()>,
}

impl RemoteClient {
    fn new(app_data: PathBuf) -> Arc<Self> {
        let desktop = fs::read(app_data.join("paired-desktop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        Arc::new(Self {
            app_data,
            endpoint: tokio::sync::RwLock::new(None),
            connection: tokio::sync::RwLock::new(None),
            desktop: tokio::sync::RwLock::new(desktop),
            start_lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn endpoint(&self) -> Result<Endpoint, String> {
        if let Some(endpoint) = self.endpoint.read().await.clone() {
            return Ok(endpoint);
        }
        let _guard = self.start_lock.lock().await;
        if let Some(endpoint) = self.endpoint.read().await.clone() {
            return Ok(endpoint);
        }
        let key = load_or_create_key(&self.app_data.join("iroh-identity"))?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(key)
            .bind()
            .await
            .map_err(|error| format!("Iroh failed to start: {error}"))?;
        *self.endpoint.write().await = Some(endpoint.clone());
        Ok(endpoint)
    }

    async fn connect(&self) -> Result<iroh::endpoint::Connection, String> {
        if let Some(connection) = self.connection.read().await.clone() {
            return Ok(connection);
        }
        let desktop = self
            .desktop
            .read()
            .await
            .clone()
            .ok_or("Pair Napstrfy with Napstr first")?;
        let address = decode_endpoint_addr(&desktop)?;
        let connection = tokio::time::timeout(
            Duration::from_secs(25),
            self.endpoint().await?.connect(address, ALPN),
        )
        .await
        .map_err(|_| "Napstr did not answer over Iroh")?
        .map_err(|error| format!("Could not reach Napstr: {error}"))?;
        *self.connection.write().await = Some(connection.clone());
        Ok(connection)
    }

    async fn pair(&self, code: &str, device_name: &str) -> Result<String, String> {
        let ticket = PairingTicket::from_uri(code)?;
        if ticket.expires_at < chrono_timestamp() {
            return Err("This pairing code has expired. Create another in Napstr.".into());
        }
        let desktop = SavedDesktop {
            endpoint_id: ticket.endpoint_id.clone(),
            endpoint_addr: ticket.endpoint_addr.clone(),
            desktop_name: ticket.desktop_name.clone(),
        };
        let endpoint = self.endpoint().await?;
        let address = decode_endpoint_addr(&desktop)?;
        let connection =
            tokio::time::timeout(Duration::from_secs(25), endpoint.connect(address, ALPN))
                .await
                .map_err(|_| "Napstr did not answer. Keep its Mobile page open and try again.")?
                .map_err(|error| format!("Could not pair over Iroh: {error}"))?;
        let response = tokio::time::timeout(
            Duration::from_secs(15),
            exchange_on(
                &connection,
                ClientRequest::Pair {
                    token: ticket.token,
                    device_name: clean_device_name(device_name),
                },
            ),
        )
        .await
        .map_err(|_| "Napstr did not complete pairing in time")??
        .0;
        let desktop_name = match response {
            ServerResponse::Paired { desktop_name } => desktop_name,
            other => return Err(unexpected_response(&other)),
        };
        let mut saved = desktop;
        saved.desktop_name = desktop_name.clone();
        save_json(&self.app_data.join("paired-desktop.json"), &saved)?;
        *self.desktop.write().await = Some(saved);
        *self.connection.write().await = Some(connection);
        Ok(desktop_name)
    }

    async fn forget(&self) -> Result<(), String> {
        *self.connection.write().await = None;
        *self.desktop.write().await = None;
        match fs::remove_file(self.app_data.join("paired-desktop.json")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn request(&self, request: ClientRequest) -> Result<ServerResponse, String> {
        let (response, _) = self.exchange(request).await?;
        match response {
            ServerResponse::Error { message } => Err(message),
            response => Ok(response),
        }
    }

    async fn exchange(
        &self,
        request: ClientRequest,
    ) -> Result<(ServerResponse, iroh::endpoint::RecvStream), String> {
        let mut last_error = "Napstr is unavailable".to_string();
        for _ in 0..2 {
            match self.connect().await {
                Ok(connection) => match tokio::time::timeout(
                    Duration::from_secs(30),
                    exchange_on(&connection, request.clone()),
                )
                .await
                {
                    Ok(Ok(response)) => return Ok(response),
                    Ok(Err(error)) => last_error = error,
                    Err(_) => last_error = "Napstr did not answer the request in time".into(),
                },
                Err(error) => last_error = error,
            }
            *self.connection.write().await = None;
        }
        Err(last_error)
    }

    async fn status(&self) -> CompanionStatus {
        let desktop = self.desktop.read().await.clone();
        if desktop.is_none() {
            return CompanionStatus {
                paired: false,
                connected: false,
                desktop_name: String::new(),
                endpoint_id: String::new(),
                library_revision: 0,
                error: String::new(),
            };
        }
        let desktop = desktop.unwrap();
        match tokio::time::timeout(Duration::from_secs(8), self.request(ClientRequest::Status))
            .await
        {
            Err(_) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error: "Napstr did not answer yet".into(),
            },
            Ok(Ok(ServerResponse::Status { library_revision })) => CompanionStatus {
                paired: true,
                connected: true,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision,
                error: String::new(),
            },
            Ok(Err(error)) if error == "invalid Napstrfy request" => {
                self.legacy_status(desktop).await
            }
            Ok(Ok(other)) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error: unexpected_response(&other),
            },
            Ok(Err(error)) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error,
            },
        }
    }

    async fn legacy_status(&self, desktop: SavedDesktop) -> CompanionStatus {
        match tokio::time::timeout(Duration::from_secs(8), self.request(ClientRequest::Ping)).await
        {
            Ok(Ok(ServerResponse::Pong)) => CompanionStatus {
                paired: true,
                connected: true,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error: String::new(),
            },
            Ok(Ok(other)) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error: unexpected_response(&other),
            },
            Ok(Err(error)) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error,
            },
            Err(_) => CompanionStatus {
                paired: true,
                connected: false,
                desktop_name: desktop.desktop_name,
                endpoint_id: desktop.endpoint_id,
                library_revision: 0,
                error: "Napstr did not answer yet".into(),
            },
        }
    }

    async fn cache_audio(
        &self,
        requested_track: RemoteTrack,
        media: Arc<MediaServer>,
        library_visible: bool,
    ) -> Result<CachedAudio, String> {
        validate_cache_track(&requested_track)?;
        let _guard = media.prepare_lock.lock().await;
        if let Some(entry) = media.entry(&requested_track.file_id).await {
            if entry.failure().is_none() {
                validate_matching_track(&requested_track, &entry.track)?;
                return Ok(CachedAudio {
                    url: media.url(&entry.track)?,
                    track: entry.track.clone(),
                });
            }
            media.entries.write().await.remove(&requested_track.file_id);
        }

        let directory = self.app_data.join("audio");
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let extension = safe_extension(&requested_track.format)?;
        let path = directory.join(format!("{}.{}", requested_track.file_id, extension));
        let temporary = directory.join(format!(".{}.part", requested_track.file_id));
        let metadata_path = directory.join(format!("{}.json", requested_track.file_id));
        if fs::metadata(&path)
            .map(|metadata| metadata.len() == requested_track.size)
            .unwrap_or(false)
        {
            save_json(
                &metadata_path,
                &CachedRemoteAudio {
                    track: requested_track.clone(),
                    library_visible,
                },
            )?;
            let entry = MediaEntry::completed(requested_track.clone(), path, temporary);
            media.insert(entry).await;
            return Ok(CachedAudio {
                url: media.url(&requested_track)?,
                track: requested_track,
            });
        }

        let (response, mut receive) = self
            .exchange(ClientRequest::FetchAudio {
                file_id: requested_track.file_id.clone(),
            })
            .await?;
        let track = match response {
            ServerResponse::AudioReady { track } => track,
            ServerResponse::Error { message } => return Err(message),
            other => return Err(unexpected_response(&other)),
        };
        validate_cache_track(&track)?;
        validate_matching_track(&requested_track, &track)?;
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_file(&temporary).await;
        let mut output = tokio::fs::File::create(&temporary)
            .await
            .map_err(|error| error.to_string())?;
        let entry = MediaEntry::downloading(track.clone(), path, temporary);
        media.insert(entry.clone()).await;
        tokio::spawn(async move {
            let result = async {
                let mut hasher = Sha256::new();
                let mut received = 0u64;
                while let Some(bytes) = receive
                    .read_chunk(256 * 1024)
                    .await
                    .map_err(|error| format!("Iroh audio stream failed: {error}"))?
                {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > entry.track.size || received > 2 * 1024 * 1024 * 1024 {
                        return Err("Napstr sent more audio data than advertised".to_string());
                    }
                    hasher.update(&bytes);
                    output
                        .write_all(&bytes)
                        .await
                        .map_err(|error| error.to_string())?;
                    entry.received.store(received, Ordering::Release);
                    entry.changed.notify_waiters();
                }
                output.flush().await.map_err(|error| error.to_string())?;
                drop(output);
                if received != entry.track.size
                    || hex::encode(hasher.finalize()) != entry.track.file_id
                {
                    return Err(
                        "Audio verification failed; the cached copy was discarded".to_string()
                    );
                }
                tokio::fs::rename(&entry.temporary_path, &entry.final_path)
                    .await
                    .map_err(|error| error.to_string())?;
                save_json(
                    &metadata_path,
                    &CachedRemoteAudio {
                        track: entry.track.clone(),
                        library_visible,
                    },
                )?;
                entry.complete.store(true, Ordering::Release);
                entry.changed.notify_waiters();
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = result {
                let _ = tokio::fs::remove_file(&entry.temporary_path).await;
                entry.fail(error);
            }
        });
        Ok(CachedAudio {
            url: media.url(&track)?,
            track,
        })
    }

    async fn cached_entries(&self) -> Result<Vec<CachedRemoteAudio>, String> {
        let app_data = self.app_data.clone();
        tokio::task::spawn_blocking(move || cached_entries_in(&app_data))
            .await
            .map_err(|error| format!("could not read the offline audio cache: {error}"))?
    }

    async fn offline_library(&self) -> Result<OfflineLibrary, String> {
        let cached = self.cached_entries().await?;
        let desktop = self.desktop.read().await.clone();
        let tracks = cached
            .into_iter()
            .filter(|item| item.library_visible)
            .map(|item| item.track)
            .collect::<Vec<_>>();
        Ok(OfflineLibrary {
            total: tracks.len(),
            tracks,
            paired: desktop.is_some(),
            desktop_name: desktop.map(|item| item.desktop_name).unwrap_or_default(),
        })
    }

    async fn reconcile_cache(
        &self,
        media: Arc<MediaServer>,
        protected_file_ids: HashSet<String>,
    ) -> Result<bool, String> {
        let cached = self.cached_entries().await?;
        if cached.is_empty() {
            return Ok(true);
        }
        let mut available = HashSet::new();
        let file_ids = cached
            .iter()
            .map(|item| item.track.file_id.clone())
            .collect::<Vec<_>>();
        for batch in file_ids.chunks(200) {
            match self
                .request(ClientRequest::Available {
                    file_ids: batch.to_vec(),
                })
                .await?
            {
                ServerResponse::Available { file_ids } => available.extend(file_ids),
                response => return Err(unexpected_response(&response)),
            }
        }

        let directory = self.app_data.join("audio");
        let mut deferred = false;
        for item in cached {
            let file_id = &item.track.file_id;
            if available.contains(file_id) {
                continue;
            }
            if protected_file_ids.contains(file_id) {
                deferred = true;
                continue;
            }
            media.entries.write().await.remove(file_id);
            let extension = safe_extension(&item.track.format)?;
            remove_if_present(&directory.join(format!("{file_id}.{extension}")))?;
            remove_if_present(&directory.join(format!(".{file_id}.part")))?;
            remove_if_present(&directory.join(format!("{file_id}.json")))?;
        }
        Ok(!deferred)
    }
}

fn cached_entries_in(app_data: &Path) -> Result<Vec<CachedRemoteAudio>, String> {
    let directory = app_data.join("audio");
    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(Vec::new());
    };
    let mut cached = Vec::new();
    for entry in entries
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .take(10_000)
    {
        let path = entry.path();
        if entry
            .metadata()
            .map(|metadata| metadata.len() > 64 * 1024)
            .unwrap_or(true)
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(item) = serde_json::from_slice::<CachedRemoteAudio>(&bytes) else {
            continue;
        };
        if stem != item.track.file_id || validate_cache_track(&item.track).is_err() {
            continue;
        }
        let Ok(extension) = safe_extension(&item.track.format) else {
            continue;
        };
        let audio_path = directory.join(format!("{}.{}", item.track.file_id, extension));
        if fs::metadata(audio_path)
            .map(|metadata| metadata.is_file() && metadata.len() == item.track.size)
            .unwrap_or(false)
        {
            cached.push(item);
        }
    }
    cached.sort_by(|left, right| {
        left.track
            .title
            .to_ascii_lowercase()
            .cmp(&right.track.title.to_ascii_lowercase())
            .then_with(|| left.track.filename.cmp(&right.track.filename))
    });
    Ok(cached)
}

struct AppState {
    remote: Arc<RemoteClient>,
    media: Arc<MediaServer>,
    podcasts: Arc<PodcastStore>,
}

#[tauri::command]
async fn companion_status(state: State<'_, AppState>) -> Result<CompanionStatus, String> {
    Ok(state.remote.status().await)
}

#[tauri::command]
async fn pair_desktop(
    code: String,
    device_name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.remote.pair(&code, &device_name).await
}

#[tauri::command]
async fn forget_desktop(state: State<'_, AppState>) -> Result<(), String> {
    state.remote.forget().await
}

#[tauri::command]
async fn remote_library(
    query: String,
    offset: usize,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<LibraryPage, String> {
    match state
        .remote
        .request(ClientRequest::Library {
            query,
            offset,
            limit,
        })
        .await?
    {
        ServerResponse::Library { tracks, total } => Ok(LibraryPage { tracks, total }),
        response => Err(unexpected_response(&response)),
    }
}

#[tauri::command]
async fn cached_library(state: State<'_, AppState>) -> Result<OfflineLibrary, String> {
    state.remote.offline_library().await
}

#[tauri::command]
async fn reconcile_audio_cache(
    protected_file_ids: Vec<String>,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let protected = protected_file_ids
        .into_iter()
        .filter(|file_id| validate_file_id(file_id).is_ok())
        .collect();
    state
        .remote
        .reconcile_cache(state.media.clone(), protected)
        .await
}

#[tauri::command]
async fn remote_search(
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<RemoteTrack>, String> {
    match state
        .remote
        .request(ClientRequest::Search { query })
        .await?
    {
        ServerResponse::Search { tracks } => Ok(tracks),
        response => Err(unexpected_response(&response)),
    }
}

#[tauri::command]
async fn remote_audiobooks(
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<RemoteAudiobook>, String> {
    match state
        .remote
        .request(ClientRequest::Audiobooks { query })
        .await?
    {
        ServerResponse::Audiobooks { audiobooks } => Ok(audiobooks),
        response => Err(unexpected_response(&response)),
    }
}

#[tauri::command]
async fn remote_audiobook_library(
    query: String,
    offset: usize,
    limit: usize,
    state: State<'_, AppState>,
) -> Result<AudiobookLibraryPage, String> {
    match state
        .remote
        .request(ClientRequest::AudiobookLibrary {
            query: query.clone(),
            offset,
            limit,
        })
        .await?
    {
        ServerResponse::AudiobookLibrary { audiobooks, total } => {
            Ok(AudiobookLibraryPage { audiobooks, total })
        }
        _ => {
            // Compatibility with Napstr versions that predate paged summaries.
            let audiobooks = remote_audiobooks(query, state).await?;
            let total = audiobooks.len();
            let audiobooks = audiobooks
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|book| RemoteAudiobookSummary {
                    audiobook_id: book.audiobook_id,
                    title: book.title,
                    author: book.author,
                    narrator: book.narrator,
                    total_size: book.total_size,
                    chapter_count: book.chapters.len(),
                })
                .collect();
            Ok(AudiobookLibraryPage { audiobooks, total })
        }
    }
}

#[tauri::command]
async fn remote_audiobook(
    audiobook_id: String,
    state: State<'_, AppState>,
) -> Result<RemoteAudiobook, String> {
    match state
        .remote
        .request(ClientRequest::Audiobook {
            audiobook_id: audiobook_id.clone(),
        })
        .await?
    {
        ServerResponse::Audiobook { audiobook } => Ok(audiobook),
        _ => remote_audiobooks(String::new(), state)
            .await?
            .into_iter()
            .find(|book| book.audiobook_id == audiobook_id)
            .ok_or("That audiobook is no longer available".into()),
    }
}

#[tauri::command]
fn podcast_parse_search(payload: String, limit: usize) -> Result<Vec<PodcastFeed>, String> {
    parse_public_podcast_search(&payload, limit)
}

#[tauri::command]
async fn podcast_episodes(
    feed: PodcastFeed,
    directory_payload: String,
    state: State<'_, AppState>,
) -> Result<Vec<PodcastEpisode>, String> {
    let episodes = parse_public_podcast_episodes(&feed, &directory_payload, 50)?;
    if !episodes.is_empty() {
        return Ok(episodes);
    }
    state.podcasts.feed_episodes(&feed, 50).await
}

#[tauri::command]
async fn podcast_download(
    episode: PodcastEpisode,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.podcasts.start(episode).await
}

#[tauri::command]
async fn podcast_downloads(state: State<'_, AppState>) -> Result<Vec<PodcastDownload>, String> {
    Ok(state.podcasts.list().await)
}

#[tauri::command]
async fn podcast_playback_url(
    episode: PodcastEpisode,
    state: State<'_, AppState>,
) -> Result<CachedPodcastAudio, String> {
    state
        .podcasts
        .playback_url(episode, state.media.clone())
        .await
}

#[tauri::command]
async fn remote_download(
    file_id: String,
    source_pubkeys: Vec<String>,
    destination_folder: Option<String>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    match state
        .remote
        .request(ClientRequest::RequestDownload {
            file_id,
            source_pubkeys,
            destination_folder,
        })
        .await?
    {
        ServerResponse::DownloadRequested { request_id } => Ok(request_id),
        response => Err(unexpected_response(&response)),
    }
}

#[tauri::command]
async fn remote_transfers(state: State<'_, AppState>) -> Result<Vec<RemoteTransfer>, String> {
    match state.remote.request(ClientRequest::Transfers).await? {
        ServerResponse::Transfers { transfers } => Ok(transfers),
        response => Err(unexpected_response(&response)),
    }
}

#[tauri::command]
async fn cache_remote_audio(
    track: RemoteTrack,
    library_visible: Option<bool>,
    state: State<'_, AppState>,
) -> Result<CachedAudio, String> {
    state
        .remote
        .cache_audio(track, state.media.clone(), library_visible.unwrap_or(true))
        .await
}

#[tauri::command]
async fn prefetch_remote_audio(
    after_file_id: String,
    track: RemoteTrack,
    library_visible: Option<bool>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_file_id(&after_file_id)?;
    validate_cache_track(&track)?;
    let key = (after_file_id.clone(), track.file_id.clone());
    if !state
        .media
        .scheduled_prefetches
        .lock()
        .await
        .insert(key.clone())
    {
        return Ok(());
    }
    let remote = state.remote.clone();
    let media = state.media.clone();
    tauri::async_runtime::spawn(async move {
        if media.wait_until_complete(&after_file_id).await.is_ok() {
            let _ = remote
                .cache_audio(track, media.clone(), library_visible.unwrap_or(true))
                .await;
        }
        media.scheduled_prefetches.lock().await.remove(&key);
    });
    Ok(())
}

async fn exchange_on(
    connection: &iroh::endpoint::Connection,
    request: ClientRequest,
) -> Result<(ServerResponse, iroh::endpoint::RecvStream), String> {
    let (mut send, mut receive) = connection
        .open_bi()
        .await
        .map_err(|error| format!("Could not open an Iroh request: {error}"))?;
    let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err("The request is too large".into());
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .map_err(|error| error.to_string())?;
    send.write_all(&payload)
        .await
        .map_err(|error| error.to_string())?;
    send.finish().map_err(|error| error.to_string())?;
    let response = read_response(&mut receive).await?;
    Ok((response, receive))
}

async fn read_response(receive: &mut iroh::endpoint::RecvStream) -> Result<ServerResponse, String> {
    let mut length = [0u8; 4];
    receive
        .read_exact(&mut length)
        .await
        .map_err(|error| format!("Could not read Napstr's response: {error}"))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_CONTROL_FRAME_BYTES {
        return Err("Napstr returned an invalid response size".into());
    }
    let mut payload = vec![0u8; length];
    receive
        .read_exact(&mut payload)
        .await
        .map_err(|error| format!("Could not read Napstr's response: {error}"))?;
    serde_json::from_slice(&payload).map_err(|_| "Napstr returned an invalid response".into())
}

fn decode_endpoint_addr(desktop: &SavedDesktop) -> Result<EndpointAddr, String> {
    serde_json::from_str(&desktop.endpoint_addr)
        .or_else(|_| {
            desktop
                .endpoint_id
                .parse::<EndpointId>()
                .map(EndpointAddr::new)
                .map_err(|_| serde_json::Error::io(std::io::Error::other("invalid endpoint ID")))
        })
        .map_err(|_| "The saved Napstr Iroh address is invalid".into())
}

fn validate_file_id(value: &str) -> Result<(), String> {
    if hex::decode(value)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err("invalid SHA-256 file ID".into())
    }
}

fn safe_extension(format: &str) -> Result<&'static str, String> {
    match format.to_ascii_uppercase().as_str() {
        "MP3" => Ok("mp3"),
        "FLAC" => Ok("flac"),
        "WAV" => Ok("wav"),
        "OGG" => Ok("ogg"),
        "OPUS" => Ok("opus"),
        "M4A" => Ok("m4a"),
        _ => Err("Napstr returned an unsupported audio format".into()),
    }
}

fn audio_mime(extension: &str) -> Option<&'static str> {
    match extension {
        "mp3" => Some("audio/mpeg"),
        "flac" => Some("audio/flac"),
        "wav" => Some("audio/wav"),
        "ogg" | "opus" => Some("audio/ogg"),
        "m4a" => Some("audio/mp4"),
        _ => None,
    }
}

fn validate_cache_track(track: &RemoteTrack) -> Result<(), String> {
    validate_file_id(&track.file_id)?;
    safe_extension(&track.format)?;
    if !track.local || track.size == 0 || track.size > 2 * 1024 * 1024 * 1024 {
        return Err("Napstr returned an invalid local audio track".into());
    }
    Ok(())
}

fn validate_matching_track(expected: &RemoteTrack, actual: &RemoteTrack) -> Result<(), String> {
    if expected.file_id != actual.file_id
        || expected.size != actual.size
        || !expected.format.eq_ignore_ascii_case(&actual.format)
        || expected.mime != actual.mime
    {
        return Err("Napstr returned different audio than Napstrfy requested".into());
    }
    Ok(())
}

fn requested_audio_range(value: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') || len == 0 {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(Some((len.saturating_sub(suffix), len - 1)));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= len {
        return Err(());
    }
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(len - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
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

fn chrono_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unexpected_response(response: &ServerResponse) -> String {
    match response {
        ServerResponse::Error { message } => message.clone(),
        _ => "Napstr returned an unexpected response".into(),
    }
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Iroh intentionally uses reqwest's bring-your-own-provider Rustls mode.
    // Mobile processes do not install a provider on our behalf, so do this
    // before Tauri or any Iroh background task can construct a TLS client.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tauri::Builder::default()
        .setup(|app| {
            #[cfg(mobile)]
            app.handle().plugin(tauri_plugin_barcode_scanner::init())?;
            let app_data = app
                .path()
                .app_data_dir()
                .map_err(|error| error.to_string())?;
            let podcasts = PodcastStore::new(&app_data)?;
            app.manage(AppState {
                remote: RemoteClient::new(app_data),
                media: MediaServer::start()?,
                podcasts,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            companion_status,
            pair_desktop,
            forget_desktop,
            remote_library,
            cached_library,
            reconcile_audio_cache,
            remote_search,
            remote_audiobooks,
            remote_audiobook_library,
            remote_audiobook,
            podcast_parse_search,
            podcast_episodes,
            podcast_download,
            podcast_downloads,
            podcast_playback_url,
            remote_download,
            remote_transfers,
            cache_remote_audio,
            prefetch_remote_audio
        ])
        .run(tauri::generate_context!())
        .expect("error while running Napstrfy");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hashes_and_known_audio_extensions_become_cache_paths() {
        assert!(validate_file_id(&"a".repeat(64)).is_ok());
        assert!(validate_file_id("../../secret").is_err());
        assert_eq!(safe_extension("FLAC").unwrap(), "flac");
        assert!(safe_extension("EXE").is_err());
    }

    #[test]
    fn completed_audio_cache_rehydrates_without_a_desktop_connection() {
        let root = std::env::temp_dir().join(format!(
            "napstrfy-offline-cache-{}-{}",
            std::process::id(),
            chrono_timestamp()
        ));
        let audio = root.join("audio");
        fs::create_dir_all(&audio).unwrap();
        let bytes = b"verified offline audio";
        let file_id = hex::encode(Sha256::digest(bytes));
        let track = RemoteTrack {
            file_id: file_id.clone(),
            filename: "offline.mp3".into(),
            title: "Offline".into(),
            artist: "Napstr".into(),
            album: String::new(),
            format: "MP3".into(),
            mime: "audio/mpeg".into(),
            size: bytes.len() as u64,
            tags: String::new(),
            local: true,
            sources: Vec::new(),
        };
        fs::write(audio.join(format!("{file_id}.mp3")), bytes).unwrap();
        save_json(
            &audio.join(format!("{file_id}.json")),
            &CachedRemoteAudio {
                track: track.clone(),
                library_visible: true,
            },
        )
        .unwrap();

        let cached = cached_entries_in(&root).unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].track, track);

        fs::write(audio.join(format!("{file_id}.mp3")), b"short").unwrap();
        assert!(cached_entries_in(&root).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mobile_names_drop_direction_overrides() {
        assert_eq!(clean_device_name("My\u{202e}Phone"), "MyPhone");
    }

    #[test]
    fn public_podcast_search_keeps_clean_unique_genres() {
        let payload = r#"{"results":[{"collectionId":42,"collectionName":"A Show","artistName":"A Host","feedUrl":"https://example.com/feed.xml","genres":["Technology","Podcasts","technology"],"primaryGenreName":"Science","trackCount":10}]}"#;
        let feeds = parse_public_podcast_search(payload, 10).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].genres, vec!["Science", "Technology", "Podcasts"]);
    }

    #[test]
    fn public_podcast_lookup_skips_the_show_and_keeps_audio_episodes() {
        let feed = PodcastFeed {
            id: 42,
            title: "A Show".into(),
            author: "A Host".into(),
            description: String::new(),
            feed_url: "https://example.com/feed.xml".into(),
            image: "https://example.com/show.jpg".into(),
            language: "en".into(),
            episode_count: 1,
            genres: vec!["Technology".into()],
        };
        let payload = r#"{"results":[
          {"wrapperType":"track","kind":"podcast","trackId":42,"collectionId":42,"trackName":"A Show"},
          {"wrapperType":"podcastEpisode","kind":"podcast-episode","trackId":99,"collectionId":42,"collectionName":"A Show","trackName":"First episode","description":"Hello","episodeUrl":"https://cdn.example.com/one.mp3","episodeContentType":"audio","episodeFileExtension":"mp3","trackTimeMillis":3723000,"releaseDate":"2026-08-25T12:00:00Z","artworkUrl600":"https://example.com/episode.jpg"}
        ]}"#;
        let episodes = parse_public_podcast_episodes(&feed, payload, 50).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].id, 99);
        assert_eq!(episodes[0].title, "First episode");
        assert_eq!(episodes[0].enclosure_type, "audio/mpeg");
        assert_eq!(episodes[0].duration, 3_723);
    }

    #[test]
    fn audio_ranges_are_not_truncated_to_a_preview_chunk() {
        assert_eq!(
            requested_audio_range(Some("bytes=0-"), 18_000_000),
            Ok(Some((0, 17_999_999)))
        );
        assert_eq!(
            requested_audio_range(Some("bytes=100-199"), 1_000),
            Ok(Some((100, 199)))
        );
        assert_eq!(
            requested_audio_range(Some("bytes=-50"), 1_000),
            Ok(Some((950, 999)))
        );
        assert!(requested_audio_range(Some("bytes=1000-"), 1_000).is_err());
    }

    #[test]
    fn podcast_rss_is_parsed_without_napstr_protocol_data() {
        let feed = PodcastFeed {
            id: 42,
            title: "Independent show".into(),
            author: "Host".into(),
            description: String::new(),
            feed_url: "https://example.com/feed.xml".into(),
            image: "https://example.com/show.jpg".into(),
            language: "en".into(),
            episode_count: 1,
            genres: vec!["Technology".into()],
        };
        let rss = br#"<?xml version="1.0"?><rss><channel><item>
          <guid>episode-one</guid><title>First episode</title>
          <pubDate>Tue, 25 Aug 2026 12:00:00 +0000</pubDate>
          <itunes:duration>01:02:03</itunes:duration>
          <enclosure url="https://cdn.example.com/one.mp3" type="audio/mpeg" length="1234" />
        </item></channel></rss>"#;
        let episodes = parse_podcast_feed(&feed, rss, 50).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].feed_title, "Independent show");
        assert_eq!(episodes[0].duration, 3_723);
        assert_eq!(episodes[0].enclosure_length, 1_234);
    }

    #[test]
    fn podcast_feeds_accept_relative_media_content_and_upgrade_public_http() {
        let feed = PodcastFeed {
            id: 42,
            title: "Show".into(),
            author: String::new(),
            description: String::new(),
            feed_url: "https://feeds.example.com/show/rss.xml".into(),
            image: String::new(),
            language: String::new(),
            episode_count: 2,
            genres: Vec::new(),
        };
        let rss = br#"<rss><channel>
          <item><title>Relative</title><media:content url="/audio/one.mp3" type="audio/mpeg" /></item>
          <item><title>Legacy HTTP</title><enclosure url="http://cdn.example.com/two.mp3" type="application/octet-stream" /></item>
        </channel></rss>"#;
        let episodes = parse_podcast_feed(&feed, rss, 50).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(
            episodes[0].enclosure_url,
            "https://feeds.example.com/audio/one.mp3"
        );
        assert_eq!(episodes[1].enclosure_url, "https://cdn.example.com/two.mp3");
        assert_eq!(episodes[1].enclosure_type, "audio/mpeg");
    }

    #[test]
    fn podcast_feeds_reject_private_enclosures() {
        let feed = PodcastFeed {
            id: 42,
            title: "Show".into(),
            author: String::new(),
            description: String::new(),
            feed_url: "https://example.com/feed.xml".into(),
            image: String::new(),
            language: String::new(),
            episode_count: 1,
            genres: Vec::new(),
        };
        let rss = br#"<rss><channel><item><title>Unsafe</title>
          <enclosure url="http://192.168.1.1/private.mp3" type="audio/mpeg" />
        </item></channel></rss>"#;
        assert!(parse_podcast_feed(&feed, rss, 50).unwrap().is_empty());
    }
}
