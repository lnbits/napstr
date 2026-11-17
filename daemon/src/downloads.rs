//! Headless download queue used by full-access Napstrfy devices.
use crate::{
    library::Library,
    protocol::{read_frame, write_frame, ClientFrame, ServerFrame, PROTOCOL_VERSION},
    server::{now, Offer},
    tor::{is_v3_onion, TorManager},
};
use napstr_remote_protocol::{RemoteAudiobook, RemoteSource, RemoteTrack, RemoteTransfer};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Notify, RwLock},
    task::JoinSet,
    time::timeout,
};

#[derive(Clone, Deserialize, Serialize)]
struct Job {
    id: String,
    #[serde(default)]
    sequence: u64,
    request_id: String,
    track: RemoteTrack,
    sources: Vec<String>,
    destination: Option<String>,
    progress: f64,
    status: String,
}

struct Pending {
    file_id: String,
    sources: HashSet<String>,
    offers: mpsc::Sender<Offer>,
}

pub struct Downloads {
    client: Client,
    tor: Arc<TorManager>,
    root: PathBuf,
    state_path: PathBuf,
    library: Arc<RwLock<Library>>,
    jobs: Mutex<BTreeMap<String, Job>>,
    pending: Mutex<HashMap<String, Pending>>,
    changed: Notify,
    pub rescan: Arc<Notify>,
    pub revision: Arc<AtomicU64>,
}

impl Downloads {
    pub fn new(
        client: Client,
        tor: Arc<TorManager>,
        root: PathBuf,
        data: &Path,
        library: Arc<RwLock<Library>>,
    ) -> Result<Arc<Self>, String> {
        let state_path = data.join("downloads.json");
        let mut jobs: BTreeMap<String, Job> = match fs::read(&state_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| "Invalid saved download queue")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.to_string()),
        };
        for job in jobs.values_mut() {
            if job.status != "Complete" && !job.status.starts_with("Failed:") {
                job.status = "Queued".into();
                job.progress = 0.0;
                job.request_id = uuid::Uuid::new_v4().to_string();
            }
        }
        Ok(Arc::new(Self {
            client,
            tor,
            root,
            state_path,
            library,
            jobs: Mutex::new(jobs),
            pending: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            rescan: Arc::new(Notify::new()),
            revision: Arc::new(AtomicU64::new(now() * 1000)),
        }))
    }

    fn save(&self, jobs: &BTreeMap<String, Job>) -> Result<(), String> {
        let mut temporary = tempfile::NamedTempFile::new_in(
            self.state_path.parent().ok_or("Missing queue directory")?,
        )
        .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temporary, jobs).map_err(|e| e.to_string())?;
        temporary.as_file().sync_all().map_err(|e| e.to_string())?;
        temporary
            .persist(&self.state_path)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn transfers(&self) -> Result<Vec<RemoteTransfer>, String> {
        Ok(self
            .jobs
            .lock()
            .map_err(|_| "Download queue lock failed")?
            .values()
            .map(|job| RemoteTransfer {
                id: job.id.clone(),
                file_id: job.track.file_id.clone(),
                filename: job.track.filename.clone(),
                size: job.track.size,
                progress: job.progress,
                status: job.status.clone(),
                speed: String::new(),
            })
            .collect())
    }

    pub async fn request(
        &self,
        file_id: String,
        sources: Vec<String>,
        destination: Option<String>,
    ) -> Result<String, String> {
        if !valid_id(&file_id) {
            return Err("Invalid file ID".into());
        }
        let sources = seeder_candidates(sources)?;
        if self.library.read().await.contains_key(&file_id) {
            return Err("This file is already in the local library".into());
        }
        validate_destination(destination.as_deref())?;
        // Resolve signed metadata from the requested seeders. Never trust size,
        // filename, or a destination supplied by a remote download offer.
        let authors = sources
            .iter()
            .map(|s| PublicKey::from_hex(s).map_err(|_| "Invalid seeder public key".to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let events = self
            .client
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(30421))
                    .identifier(&file_id)
                    .authors(authors)
                    .limit(100),
                Duration::from_secs(8),
            )
            .await
            .map_err(|e| e.to_string())?;
        let track = events
            .iter()
            .filter_map(catalogue_track)
            .find(|track| track.file_id == file_id)
            .ok_or("No valid catalogue entry was returned by the requested seeders")?;
        let verified_sources: Vec<_> = events
            .iter()
            .filter(|event| {
                catalogue_track(event).is_some_and(|candidate| {
                    candidate.file_id == track.file_id && candidate.size == track.size
                })
            })
            .map(|event| event.pubkey.to_hex())
            .filter(|key| sources.contains(key))
            .collect::<HashSet<_>>()
            .into_iter()
            .take(3)
            .collect();
        if verified_sources.is_empty() {
            return Err("The requested seeders no longer share this file".into());
        }
        self.enqueue(track, verified_sources, destination)
    }

    fn enqueue(
        &self,
        track: RemoteTrack,
        sources: Vec<String>,
        destination: Option<String>,
    ) -> Result<String, String> {
        let mut jobs = self.jobs.lock().map_err(|_| "Download queue lock failed")?;
        if let Some(existing) = jobs.values().find(|job| {
            job.track.file_id == track.file_id
                && job.status != "Complete"
                && !job.status.starts_with("Failed:")
        }) {
            return Ok(existing.request_id.clone());
        }
        // Keep persistent state and remote responses bounded.
        while jobs.len() >= 200 {
            let removable = jobs
                .iter()
                .find(|(_, job)| job.status == "Complete" || job.status.starts_with("Failed:"))
                .map(|(id, _)| id.clone())
                .ok_or("Download queue is full")?;
            jobs.remove(&removable);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let job = Job {
            id: id.clone(),
            sequence: jobs.values().map(|job| job.sequence).max().unwrap_or(0) + 1,
            request_id: id.clone(),
            track,
            sources,
            destination,
            progress: 0.0,
            status: "Queued".into(),
        };
        let mut updated = jobs.clone();
        updated.insert(id.clone(), job);
        self.save(&updated)?;
        *jobs = updated;
        self.changed.notify_one();
        Ok(id)
    }

    pub fn receive_offer(&self, sender: PublicKey, offer: Offer) {
        if offer.expires_at <= now()
            || !is_v3_onion(&offer.onion)
            || offer.port == 0
            || !valid_id(&offer.capability)
        {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            let id = offer.request_id.clone();
            if let Some(request) = pending.get_mut(&id) {
                if request.file_id == offer.file_id && request.sources.remove(&sender.to_hex()) {
                    let _ = request.offers.try_send(offer);
                }
                if request.sources.is_empty() {
                    pending.remove(&id);
                }
            }
        }
    }

    pub fn receive_refusal(&self, sender: PublicKey, request_id: &str, file_id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(request) = pending.get_mut(request_id) {
                if request.file_id == file_id {
                    request.sources.remove(&sender.to_hex());
                }
                if request.sources.is_empty() {
                    pending.remove(request_id);
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<(), String> {
        let mut workers = JoinSet::new();
        for _ in 0..2 {
            let service = self.clone();
            workers.spawn(async move { service.worker().await });
        }
        workers
            .join_next()
            .await
            .ok_or("Download workers stopped")?
            .map_err(|e| e.to_string())?
    }

    async fn worker(&self) -> Result<(), String> {
        loop {
            let notified = self.changed.notified();
            let next = {
                let mut jobs = self.jobs.lock().map_err(|_| "Download queue lock failed")?;
                let next = jobs
                    .values()
                    .filter(|job| job.status == "Queued")
                    .min_by_key(|job| job.sequence)
                    .map(|job| job.id.clone());
                next.and_then(|id| jobs.get_mut(&id)).map(|job| {
                    job.status = "Requesting Tor seeders".into();
                    job.clone()
                })
            };
            let Some(job) = next else {
                notified.await;
                continue;
            };
            let result = if self.library.read().await.contains_key(&job.track.file_id) {
                Ok(())
            } else {
                timeout(Duration::from_secs(2 * 60 * 60), self.fetch(&job))
                    .await
                    .unwrap_or_else(|_| Err("Download timed out".into()))
            };
            self.pending
                .lock()
                .map_err(|_| "Download request lock failed")?
                .remove(&job.request_id);
            let mut jobs = self.jobs.lock().map_err(|_| "Download queue lock failed")?;
            if let Some(saved) = jobs.get_mut(&job.id) {
                match result {
                    Ok(()) => {
                        saved.progress = 100.0;
                        saved.status = "Complete".into();
                        self.rescan.notify_one();
                    }
                    Err(error) => {
                        saved.progress = 0.0;
                        saved.status = format!("Failed: {error}");
                    }
                }
            }
            self.save(&jobs)?;
        }
    }

    async fn fetch(&self, job: &Job) -> Result<(), String> {
        let (sender, mut offers) = mpsc::channel(3);
        self.pending
            .lock()
            .map_err(|_| "Download request lock failed")?
            .insert(
                job.request_id.clone(),
                Pending {
                    file_id: job.track.file_id.clone(),
                    sources: job.sources.iter().cloned().collect(),
                    offers: sender,
                },
            );
        let content = serde_json::json!({"type":"DOWNLOAD_REQUEST", "protocol":"napstr/1", "request_id": job.request_id, "file_id": job.track.file_id}).to_string();
        let mut deliveries = JoinSet::new();
        for source in &job.sources {
            let receiver = PublicKey::from_hex(source).map_err(|_| "Invalid saved seeder")?;
            let client = self.client.clone();
            let content = content.clone();
            deliveries.spawn(async move {
                let _ = timeout(
                    Duration::from_secs(20),
                    client.send_private_msg(
                        receiver,
                        content,
                        [
                            Tag::expiration(Timestamp::from(now() + 1200)),
                            Tag::client("Napstr"),
                        ],
                    ),
                )
                .await;
            });
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let mut error = "No Tor seeder replied".to_string();
        loop {
            // Keep trying offers already received even if another seeder took
            // most of the reply window to become reachable.
            let offer = match offers.try_recv() {
                Ok(offer) => offer,
                Err(_) => match tokio::time::timeout_at(deadline, offers.recv()).await {
                    Ok(Some(offer)) => offer,
                    _ => break,
                },
            };
            if offer.expires_at <= now() {
                continue;
            }
            self.progress(&job.id, 0.0, "Connecting over Tor");
            let cancel = tokio_util::sync::CancellationToken::new();
            let connected = timeout(
                Duration::from_secs(90),
                self.tor
                    .connect_onion_with_retry(&offer.onion, offer.port, &cancel),
            )
            .await
            .unwrap_or_else(|_| Err("Seeder connection timed out".into()));
            match connected {
                Ok(mut stream) => match self.download_stream(job, &offer, &mut stream).await {
                    Ok(()) => return Ok(()),
                    Err(failure) => error = failure,
                },
                Err(failure) => error = failure,
            }
        }
        Err(error)
    }

    fn progress(&self, id: &str, progress: f64, status: &str) {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.get_mut(id) {
                job.progress = progress;
                job.status = status.into();
            }
        }
    }

    async fn download_stream<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &self,
        job: &Job,
        offer: &Offer,
        stream: &mut S,
    ) -> Result<(), String> {
        write_frame(
            stream,
            &ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capability: offer.capability.clone(),
                file_id: job.track.file_id.clone(),
            },
        )
        .await?;
        match timeout(
            Duration::from_secs(120),
            read_frame::<_, ServerFrame>(stream),
        )
        .await
        .map_err(|_| "Seeder handshake timed out")??
        {
            ServerFrame::Welcome {
                version: PROTOCOL_VERSION,
                file_id,
                size,
                ..
            } if file_id == job.track.file_id && size == job.track.size => {}
            _ => return Err("Seeder returned an invalid file description".into()),
        }
        write_frame(stream, &ClientFrame::RequestFile).await?;
        match timeout(
            Duration::from_secs(120),
            read_frame::<_, ServerFrame>(stream),
        )
        .await
        .map_err(|_| "Seeder did not send the file")??
        {
            ServerFrame::FileData { size, sha256 }
                if size == job.track.size && sha256 == job.track.file_id => {}
            _ => return Err("Seeder returned an invalid file header".into()),
        }
        let directory = destination_dir(&self.root, job.destination.as_deref())?;
        let extension = Path::new(&job.track.filename)
            .extension()
            .and_then(|s| s.to_str())
            .ok_or("Invalid audio filename")?;
        let temporary = tempfile::Builder::new()
            .prefix(".napstr-download-")
            .suffix(&format!(".{extension}.part"))
            .tempfile_in(&directory)
            .map_err(|e| e.to_string())?;
        let mut output =
            tokio::fs::File::from_std(temporary.as_file().try_clone().map_err(|e| e.to_string())?);
        let mut remaining = job.track.size;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 256 * 1024];
        while remaining > 0 {
            let length = remaining.min(buffer.len() as u64) as usize;
            let count = timeout(Duration::from_secs(120), stream.read(&mut buffer[..length]))
                .await
                .map_err(|_| "Seeder stopped sending audio")?
                .map_err(|e| e.to_string())?;
            if count == 0 {
                return Err("Incomplete audio file".into());
            }
            output
                .write_all(&buffer[..count])
                .await
                .map_err(|e| e.to_string())?;
            hash.update(&buffer[..count]);
            remaining -= count as u64;
            self.progress(
                &job.id,
                (job.track.size - remaining) as f64 * 100.0 / job.track.size as f64,
                "Downloading",
            );
        }
        output.flush().await.map_err(|e| e.to_string())?;
        output.sync_all().await.map_err(|e| e.to_string())?;
        drop(output);
        if hex::encode(hash.finalize()) != job.track.file_id {
            return Err("Audio SHA-256 verification failed".into());
        }
        let audio = crate::audio::validate_audio(temporary.path())?;
        if destination_dir(&self.root, job.destination.as_deref())? != directory {
            return Err("Download directory changed".into());
        }
        let path = persist_audio(temporary, &directory, &job.track.filename)?;
        let file = crate::library::SharedFile {
            protocol: "napstr/1",
            file_id: job.track.file_id.clone(),
            filename: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            title: audio.metadata.title,
            artist: audio.metadata.artist,
            album: audio.metadata.album,
            format: audio.format.into(),
            mime: audio.mime.into(),
            size: job.track.size,
            license: "unspecified",
            description: "",
            tags: "",
            path,
            track_number: audio.metadata.track_number,
            disc_number: audio.metadata.disc_number,
        };
        self.library
            .write()
            .await
            .insert(file.file_id.clone(), file);
        self.revision.fetch_add(1, Ordering::SeqCst);
        let _ = timeout(Duration::from_secs(10), async {
            write_frame(stream, &ClientFrame::TransferComplete).await?;
            read_frame::<_, ServerFrame>(stream).await
        })
        .await;
        Ok(())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<RemoteTrack>, String> {
        let token = catalogue_query_token(query);
        let events = self
            .client
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(30421))
                    .hashtag(token)
                    .limit(500),
                Duration::from_secs(5),
            )
            .await
            .map_err(|e| e.to_string())?;
        let availability = self.availability().await?;
        let mut tracks = BTreeMap::<String, RemoteTrack>::new();
        for event in events.iter() {
            let Some(mut track) = catalogue_track(event) else {
                continue;
            };
            if !crate::mobile::matches_query(query, &track)
                || !availability
                    .get(&event.pubkey.to_hex())
                    .is_some_and(|ids| ids.contains(&track.file_id))
            {
                continue;
            }
            let source = RemoteSource {
                pubkey: event.pubkey.to_hex(),
                display_name: event.pubkey.to_hex()[..12].into(),
            };
            if let Some(existing) = tracks.get_mut(&track.file_id) {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
            } else {
                track.sources.push(source);
                tracks.insert(track.file_id.clone(), track);
            }
        }
        Ok(tracks.into_values().take(200).collect())
    }

    pub async fn books(
        &self,
        query: &str,
        id: Option<&str>,
    ) -> Result<Vec<RemoteAudiobook>, String> {
        let mut filter = Filter::new()
            .kind(Kind::from(30423))
            .hashtag("napstr-audiobook")
            .limit(250);
        if let Some(id) = id {
            if !valid_id(id) {
                return Err("Invalid audiobook ID".into());
            }
            filter = filter.identifier(id);
        }
        let events = self
            .client
            .fetch_events(filter, Duration::from_secs(5))
            .await
            .map_err(|e| e.to_string())?;
        let available = self.availability().await?;
        let mut books = BTreeMap::new();
        for event in events.iter() {
            let Some(book) = remote_book(event) else {
                continue;
            };
            if !crate::mobile::matches_book(query, &book)
                || !available
                    .get(&event.pubkey.to_hex())
                    .is_some_and(|ids| book.chapters.iter().all(|c| ids.contains(&c.file_id)))
            {
                continue;
            }
            books.entry(book.audiobook_id.clone()).or_insert(book);
        }
        Ok(books.into_values().take(200).collect())
    }

    async fn availability(&self) -> Result<HashMap<String, HashSet<String>>, String> {
        let events = self
            .client
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(30422))
                    .hashtag("napstr-availability")
                    .limit(1000),
                Duration::from_secs(5),
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut available = HashMap::<String, HashSet<String>>::new();
        for event in events.iter() {
            if event.content.len() > 64 * 1024
                || !event.tags.expiration().is_some_and(|e| e.as_secs() > now())
            {
                continue;
            }
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&event.content) {
                available
                    .entry(event.pubkey.to_hex())
                    .or_default()
                    .extend(ids.into_iter().filter(|id| valid_id(id)).take(400));
            }
        }
        Ok(available)
    }
}

#[derive(Deserialize)]
struct CatalogueEntry {
    protocol: String,
    file_id: String,
    filename: String,
    title: String,
    artist: String,
    album: String,
    format: String,
    mime: String,
    size: u64,
    #[serde(default)]
    tags: String,
}

fn catalogue_track(event: &Event) -> Option<RemoteTrack> {
    if event.kind != Kind::from(30421) || event.content.len() > 64 * 1024 || event.verify().is_err()
    {
        return None;
    }
    let entry: CatalogueEntry = serde_json::from_str(&event.content).ok()?;
    if entry.protocol != "napstr/1"
        || !valid_id(&entry.file_id)
        || event.tags.identifier() != Some(entry.file_id.as_str())
        || entry.size == 0
        || !valid_audio_claim(&entry.filename, &entry.format, &entry.mime)
        || [
            &entry.filename,
            &entry.title,
            &entry.artist,
            &entry.album,
            &entry.tags,
        ]
        .iter()
        .any(|text| text.chars().count() > 256 || text.chars().any(char::is_control))
    {
        return None;
    }
    Some(RemoteTrack {
        file_id: entry.file_id,
        filename: entry.filename,
        title: entry.title,
        artist: entry.artist,
        album: entry.album,
        format: entry.format,
        mime: entry.mime,
        size: entry.size,
        tags: entry.tags,
        local: false,
        sources: Vec::new(),
    })
}

// Match desktop selection: companion requests may list every known seeder.
fn seeder_candidates(mut sources: Vec<String>) -> Result<Vec<String>, String> {
    sources.sort();
    sources.dedup();
    sources.truncate(3);
    if sources.is_empty() {
        return Err("Choose at least one seeder".into());
    }
    Ok(sources)
}

fn catalogue_query_token(query: &str) -> String {
    // These words are deliberately omitted from desktop catalogue search tags.
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "audio", "be", "by", "flac", "for", "from", "in",
        "is", "it", "mp3", "of", "ogg", "on", "opus", "or", "the", "to", "wav", "with",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .find(|s| (2..=32).contains(&s.chars().count()) && !STOP_WORDS.contains(&s.as_str()))
        .unwrap_or_else(|| "napstr".into())
}

fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn valid_audio_claim(filename: &str, format: &str, mime: &str) -> bool {
    if filename.contains(['/', '\\'])
        || Path::new(filename).file_name().and_then(|s| s.to_str()) != Some(filename)
    {
        return false;
    }
    let extension = Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
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
fn validate_destination(destination: Option<&str>) -> Result<(), String> {
    if let Some(name) = destination {
        if name.is_empty()
            || name.chars().count() > 200
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
            || !matches!(
                Path::new(name).components().collect::<Vec<_>>().as_slice(),
                [Component::Normal(_)]
            )
        {
            return Err("Invalid audiobook destination folder".into());
        }
    }
    Ok(())
}
fn destination_dir(root: &Path, destination: Option<&str>) -> Result<PathBuf, String> {
    validate_destination(destination)?;
    let canonical = root.canonicalize().map_err(|e| e.to_string())?;
    if canonical != root {
        return Err("Music directory changed".into());
    }
    let Some(destination) = destination else {
        return Ok(canonical);
    };
    let mut directory = canonical;
    for component in ["Audiobooks", destination] {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory).map_err(|e| e.to_string())?
            }
            _ => return Err("Download destination must be a real directory".into()),
        }
        if directory.canonicalize().map_err(|e| e.to_string())? != directory {
            return Err("Download destination contains a symlink".into());
        }
    }
    Ok(directory)
}
fn persist_audio(
    mut temporary: tempfile::NamedTempFile,
    directory: &Path,
    filename: &str,
) -> Result<PathBuf, String> {
    let name = Path::new(filename);
    let stem = name
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("Invalid audio filename")?;
    let extension = name
        .extension()
        .and_then(|s| s.to_str())
        .ok_or("Invalid audio filename")?;
    for index in 0..1000 {
        let target = directory.join(if index == 0 {
            filename.to_owned()
        } else {
            format!("{stem} ({index}).{extension}")
        });
        match temporary.persist_noclobber(&target) {
            Ok(_) => return Ok(target),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                temporary = error.file
            }
            Err(error) => return Err(error.error.to_string()),
        }
    }
    Err("Too many files with the same name".into())
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;

include!("remote_book.rs");
