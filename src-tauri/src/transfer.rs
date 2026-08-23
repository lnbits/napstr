use crate::{
    protocol::{read_frame, write_frame, ClientFrame, ServerFrame, PROTOCOL_VERSION},
    tor::{is_v3_onion, OnionLease, TorManager},
};
use chrono::Utc;
use rand::RngCore;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    fs::{self, File},
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpListener,
    sync::{watch, Mutex, RwLock, Semaphore},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedingStat {
    pub file_id: String,
    pub delivered: u32,
    pub active_grants: u32,
    pub other_seeders: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadOffer {
    pub request_id: String,
    pub file_id: String,
    pub onion: String,
    pub port: u16,
    pub capability: String,
    pub expires_at: i64,
}

struct SessionGrant {
    file_id: String,
    requester: String,
    expires_at: i64,
    _onion_lease: Option<Arc<OnionLease>>,
}

struct ListenerRuntime {
    port: u16,
    _task: JoinHandle<()>,
}

pub struct TransferService {
    db_path: PathBuf,
    tor: Arc<TorManager>,
    grants: Arc<RwLock<HashMap<String, SessionGrant>>>,
    listener: Mutex<Option<ListenerRuntime>>,
    session_onion: Mutex<Option<Arc<OnionLease>>>,
    active: Arc<Mutex<HashMap<String, Arc<DownloadCoordinator>>>>,
    globally_paused: AtomicBool,
}

struct DownloadCoordinator {
    cancel: CancellationToken,
    paused: watch::Sender<bool>,
    destination: Mutex<Option<PathBuf>>,
    primary_slot: Arc<Semaphore>,
    workers: AtomicUsize,
    complete: AtomicBool,
}

impl DownloadCoordinator {
    fn new(initially_paused: bool) -> Arc<Self> {
        let (paused, _) = watch::channel(initially_paused);
        Arc::new(Self {
            cancel: CancellationToken::new(),
            paused,
            destination: Mutex::new(None),
            primary_slot: Arc::new(Semaphore::new(1)),
            workers: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
        })
    }
}

impl TransferService {
    pub fn new(db_path: PathBuf, tor: Arc<TorManager>) -> Self {
        Self {
            db_path,
            tor,
            grants: Arc::new(RwLock::new(HashMap::new())),
            listener: Mutex::new(None),
            session_onion: Mutex::new(None),
            active: Arc::new(Mutex::new(HashMap::new())),
            globally_paused: AtomicBool::new(false),
        }
    }

    pub async fn warm_tor(&self) -> Result<(), String> {
        self.tor.start().await.map(|_| ())
    }

    pub async fn seeding_stats(&self) -> Result<Vec<SeedingStat>, String> {
        let now = Utc::now().timestamp();
        let mut active: HashMap<String, u32> = HashMap::new();
        for grant in self.grants.read().await.values() {
            if grant.expires_at > now {
                *active.entry(grant.file_id.clone()).or_default() += 1;
            }
        }
        let connection = crate::open_connection(&self.db_path)?;
        let mut statement = connection
            .prepare("SELECT file_id, delivered FROM upload_stats")
            .map_err(|error| error.to_string())?;
        let mut stats: HashMap<String, SeedingStat> = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .map(|(file_id, delivered)| {
                let stat = SeedingStat {
                    file_id: file_id.clone(),
                    delivered: delivered.max(0) as u32,
                    active_grants: 0,
                    other_seeders: 0,
                };
                (file_id, stat)
            })
            .collect();
        for (file_id, count) in active {
            stats
                .entry(file_id.clone())
                .or_insert_with(|| SeedingStat {
                    file_id,
                    delivered: 0,
                    active_grants: 0,
                    other_seeders: 0,
                })
                .active_grants = count;
        }
        let mut list: Vec<SeedingStat> = stats.into_values().collect();
        list.sort_by(|left, right| left.file_id.cmp(&right.file_id));
        Ok(list)
    }

    pub async fn warm_for_sharing(&self) -> Result<(), String> {
        let port = self.ensure_listener().await?;
        let mut session_onion = self.session_onion.lock().await;
        if session_onion.is_none() {
            *session_onion = Some(self.tor.create_onion(port).await?);
        }
        Ok(())
    }

    pub async fn create_offer(
        &self,
        request_id: String,
        file_id: String,
        requester: String,
    ) -> Result<DownloadOffer, String> {
        let connection = crate::open_connection(&self.db_path)?;
        let path: Option<String> = connection
            .query_row(
                "SELECT path FROM files WHERE file_id = ?1 AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
                [&file_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let requester_blocked: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blocked_pubkeys WHERE pubkey=?1)",
                [&requester],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if requester_blocked {
            return Err("requester is blocked".into());
        }
        let Some(path) = path else {
            return Err("requested file ID is not currently shared".into());
        };
        crate::audio::validate_audio(Path::new(&path))
            .map_err(|error| format!("shared file failed the audio-only policy: {error}"))?;
        {
            let now = Utc::now().timestamp();
            let mut grants = self.grants.write().await;
            grants.retain(|_, grant| grant.expires_at > now);
            if grants.len() >= 64 {
                return Err(
                    "this peer is already serving the maximum number of private offers".into(),
                );
            }
            if grants
                .values()
                .any(|grant| grant.file_id == file_id && grant.requester == requester)
            {
                return Err("an offer for this requester and file is already active".into());
            }
        }

        let port = self.ensure_listener().await?;
        let onion_lease = {
            let mut session_onion = self.session_onion.lock().await;
            if session_onion.is_none() {
                *session_onion = Some(self.tor.create_onion(port).await?);
            }
            session_onion
                .as_ref()
                .cloned()
                .ok_or("Tor session onion disappeared")?
        };
        let mut random = [0u8; 32];
        rand::rng().fill_bytes(&mut random);
        let capability = hex::encode(random);
        let key = capability_key(&capability);
        let expires_at = Utc::now().timestamp() + 15 * 60;
        self.grants.write().await.insert(
            key,
            SessionGrant {
                file_id: file_id.clone(),
                requester,
                expires_at,
                _onion_lease: Some(onion_lease.clone()),
            },
        );
        let grants = self.grants.clone();
        let expiring_key = capability_key(&capability);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15 * 60 + 1)).await;
            grants.write().await.remove(&expiring_key);
        });
        Ok(DownloadOffer {
            request_id,
            file_id,
            onion: onion_lease.onion.clone(),
            port: 80,
            capability,
            expires_at,
        })
    }

    async fn ensure_listener(&self) -> Result<u16, String> {
        let mut guard = self.listener.lock().await;
        if let Some(runtime) = guard.as_ref() {
            return Ok(runtime.port);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let db_path = self.db_path.clone();
        let grants = self.grants.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let db_path = db_path.clone();
                let grants = grants.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, &db_path, grants).await;
                });
            }
        });
        *guard = Some(ListenerRuntime { port, _task: task });
        Ok(port)
    }

    pub async fn accept_offer(
        &self,
        offer: DownloadOffer,
        source_pubkey: String,
    ) -> Result<(), String> {
        if offer.expires_at <= Utc::now().timestamp() {
            return Err("download offer has expired".into());
        }
        if !is_v3_onion(&offer.onion) {
            return Err("refusing a download offer without a valid Tor v3 onion".into());
        }
        let status: Option<String> = crate::open_connection(&self.db_path)?
            .query_row(
                "SELECT status FROM network_downloads WHERE request_id=?1",
                [&offer.request_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if matches!(
            status.as_deref(),
            None | Some("Verified · Complete") | Some("Cancelled")
        ) {
            return Ok(());
        }
        let db_path = self.db_path.clone();
        let tor = self.tor.clone();
        let active = self.active.clone();
        let request_id = offer.request_id.clone();
        let coordinator = {
            let mut active = self.active.lock().await;
            let initially_paused = self.globally_paused.load(Ordering::SeqCst);
            active
                .entry(request_id.clone())
                .or_insert_with(|| DownloadCoordinator::new(initially_paused))
                .clone()
        };
        coordinator.workers.fetch_add(1, Ordering::SeqCst);
        let pause_rx = coordinator.paused.subscribe();
        tokio::spawn(async move {
            let result = download_offer(
                &db_path,
                tor,
                &offer,
                &source_pubkey,
                coordinator.clone(),
                pause_rx,
            )
            .await;
            let remaining = coordinator.workers.fetch_sub(1, Ordering::SeqCst) - 1;
            let source_status = match &result {
                Ok(_) => "Complete".to_string(),
                Err(error) => format!("Failed: {error}"),
            };
            if let Ok(connection) = crate::open_connection(&db_path) {
                let _ = connection.execute("UPDATE download_sources SET status=?1,updated_at=?2 WHERE request_id=?3 AND source_pubkey=?4", params![source_status, Utc::now().to_rfc3339(), offer.request_id, source_pubkey]);
            }
            if let Err(error) = result {
                if remaining == 0 && !coordinator.complete.load(Ordering::SeqCst) {
                    let status = if coordinator.cancel.is_cancelled() {
                        "Cancelled".to_string()
                    } else {
                        format!("Failed: {error}")
                    };
                    let current = current_progress(&db_path, &offer.request_id).unwrap_or(0.0);
                    let _ =
                        update_download(&db_path, &offer.request_id, current, &status, "—", None);
                }
            }
            if remaining == 0 {
                active.lock().await.remove(&request_id);
            }
        });
        Ok(())
    }

    pub async fn set_paused(&self, paused: bool) {
        self.globally_paused.store(paused, Ordering::SeqCst);
        for transfer in self.active.lock().await.values() {
            let _ = transfer.paused.send(paused);
        }
    }

    pub async fn cancel_by_rowid(&self, rowid: i64) -> Result<(), String> {
        let connection = crate::open_connection(&self.db_path)?;
        let request_id: Option<String> = connection
            .query_row(
                "SELECT request_id FROM network_downloads WHERE rowid=?1",
                [rowid],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(request_id) = request_id {
            if let Some(transfer) = self.active.lock().await.get(&request_id) {
                transfer.cancel.cancel();
            }
            let progress = current_progress(&self.db_path, &request_id).unwrap_or(0.0);
            update_download(&self.db_path, &request_id, progress, "Cancelled", "—", None)?;
        }
        Ok(())
    }
}

fn capability_key(capability: &str) -> String {
    hex::encode(Sha256::digest(capability.as_bytes()))
}

async fn serve_connection<S>(
    mut stream: S,
    db_path: &Path,
    grants: Arc<RwLock<HashMap<String, SessionGrant>>>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hello: ClientFrame = timeout(Duration::from_secs(30), read_frame(&mut stream))
        .await
        .map_err(|_| "HELLO timed out".to_string())??;
    let (capability, file_id) = match hello {
        ClientFrame::Hello {
            version,
            capability,
            file_id,
        } if version == PROTOCOL_VERSION => (capability, file_id),
        _ => {
            write_frame(
                &mut stream,
                &ServerFrame::Error {
                    code: "BAD_HELLO".into(),
                    message: "protocol negotiation failed".into(),
                },
            )
            .await?;
            return Err("invalid HELLO".into());
        }
    };
    let key = capability_key(&capability);
    let authorized = {
        let guard = grants.read().await;
        guard
            .get(&key)
            .map(|grant| grant.file_id == file_id && grant.expires_at > Utc::now().timestamp())
            .unwrap_or(false)
    };
    if !authorized {
        write_frame(
            &mut stream,
            &ServerFrame::Error {
                code: "UNAUTHORIZED".into(),
                message: "capability is invalid or expired".into(),
            },
        )
        .await?;
        return Err("invalid capability".into());
    }

    let connection = crate::open_connection(db_path)?;
    let record: Option<(String, String, i64)> = connection
        .query_row(
            "SELECT path, filename, size FROM files WHERE file_id = ?1 AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
            [&file_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (path, filename, size) = record.ok_or("shared file disappeared")?;
    crate::audio::validate_audio(Path::new(&path))
        .map_err(|error| format!("shared file failed the audio-only policy: {error}"))?;
    write_frame(
        &mut stream,
        &ServerFrame::Welcome {
            version: PROTOCOL_VERSION,
            file_id: file_id.clone(),
            filename,
            size: size as u64,
        },
    )
    .await?;

    let mut file = File::open(path).await.map_err(|error| error.to_string())?;
    let mut sent_file = false;
    loop {
        match timeout(
            Duration::from_secs(15 * 60),
            read_frame::<_, ClientFrame>(&mut stream),
        )
        .await
        .map_err(|_| "peer was idle for too long".to_string())??
        {
            ClientFrame::RequestFile if !sent_file => {
                write_frame(
                    &mut stream,
                    &ServerFrame::FileData {
                        size: size as u64,
                        sha256: file_id.clone(),
                    },
                )
                .await?;
                let mut remaining = size as u64;
                let mut hasher = Sha256::new();
                let mut buffer = vec![0u8; 256 * 1024];
                while remaining > 0 {
                    let wanted = remaining.min(buffer.len() as u64) as usize;
                    let count = file
                        .read(&mut buffer[..wanted])
                        .await
                        .map_err(|error| error.to_string())?;
                    if count == 0 {
                        return Err("shared file became shorter while streaming".into());
                    }
                    hasher.update(&buffer[..count]);
                    stream
                        .write_all(&buffer[..count])
                        .await
                        .map_err(|error| error.to_string())?;
                    remaining -= count as u64;
                }
                stream.flush().await.map_err(|error| error.to_string())?;
                if hex::encode(hasher.finalize()) != file_id {
                    return Err("local shared file changed after indexing".into());
                }
                sent_file = true;
            }
            ClientFrame::RequestFile => return Err("duplicate file request".into()),
            ClientFrame::TransferComplete => {
                write_frame(&mut stream, &ServerFrame::TransferComplete).await?;
                grants.write().await.remove(&key);
                if sent_file {
                    let _ = record_delivery(db_path, &file_id);
                }
                return Ok(());
            }
            ClientFrame::Cancel => {
                grants.write().await.remove(&key);
                return Ok(());
            }
            ClientFrame::Hello { .. } => return Err("duplicate HELLO".into()),
        }
    }
}

async fn download_offer(
    db_path: &Path,
    tor: Arc<TorManager>,
    offer: &DownloadOffer,
    source_pubkey: &str,
    coordinator: Arc<DownloadCoordinator>,
    mut pause: watch::Receiver<bool>,
) -> Result<(), String> {
    let existing_progress = current_progress(db_path, &offer.request_id).unwrap_or(0.0);
    update_source_status(
        db_path,
        &offer.request_id,
        source_pubkey,
        "Racing Tor connection",
    )?;
    if coordinator.primary_slot.available_permits() > 0 {
        update_download(
            db_path,
            &offer.request_id,
            existing_progress,
            "Racing responsive Tor seeders",
            "Connecting…",
            Some(&offer.onion),
        )?;
    }
    let mut stream = tor
        .connect_onion_with_retry(&offer.onion, offer.port, &coordinator.cancel)
        .await?;
    write_frame(
        &mut stream,
        &ClientFrame::Hello {
            version: PROTOCOL_VERSION,
            capability: offer.capability.clone(),
            file_id: offer.file_id.clone(),
        },
    )
    .await?;
    let welcome: ServerFrame = timeout(Duration::from_secs(60), read_frame(&mut stream))
        .await
        .map_err(|_| "peer manifest timed out".to_string())??;
    let size = match welcome {
        ServerFrame::Welcome {
            version,
            file_id,
            size,
            ..
        } if version == PROTOCOL_VERSION && file_id == offer.file_id => size,
        ServerFrame::Error { message, .. } => return Err(message),
        _ => return Err("peer returned an invalid manifest".into()),
    };
    let (filename, expected_size): (String, i64) = crate::open_connection(db_path)?
        .query_row(
            "SELECT filename,size FROM network_downloads WHERE request_id=?1 AND file_id=?2",
            params![offer.request_id, offer.file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    if size != expected_size as u64 {
        return Err("seeder file size did not match the signed catalogue".into());
    }

    // Connections and manifests race in parallel, but only the first valid
    // source is allowed to carry file bytes. Other connected sources wait here
    // as cheap fallbacks and are promoted automatically if the primary fails.
    update_source_status(db_path, &offer.request_id, source_pubkey, "Ready fallback")?;
    let _primary = tokio::select! {
        permit = coordinator.primary_slot.clone().acquire_owned() => {
            permit.map_err(|_| "download source selector closed".to_string())?
        }
        _ = coordinator.cancel.cancelled() => return Err("cancelled".into()),
    };
    if coordinator.complete.load(Ordering::SeqCst) {
        let _ = write_frame(&mut stream, &ClientFrame::Cancel).await;
        return Ok(());
    }
    update_source_status(db_path, &offer.request_id, source_pubkey, "Primary")?;
    let previous_progress = current_progress(db_path, &offer.request_id).unwrap_or(0.0);
    let source_status = if previous_progress > 0.0 {
        "Primary seeder failed · restarting with backup"
    } else {
        "Downloading from fastest responsive Tor seeder"
    };
    update_download(
        db_path,
        &offer.request_id,
        0.0,
        source_status,
        "Starting…",
        Some(&offer.onion),
    )?;

    let connection = crate::open_connection(db_path)?;
    let napstr_folder = PathBuf::from(super::get_setting(&connection, "shared_folder")?);
    fs::create_dir_all(&napstr_folder)
        .await
        .map_err(|error| error.to_string())?;
    let destination = {
        let mut selected = coordinator.destination.lock().await;
        selected
            .get_or_insert_with(|| super::unique_destination(&napstr_folder, &filename))
            .clone()
    };
    drop(connection);
    let partial = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ));
    let final_result: Result<(), String> = async {
        while *pause.borrow() {
            update_download(
                db_path,
                &offer.request_id,
                0.0,
                "Paused",
                "—",
                Some(&offer.onion),
            )?;
            tokio::select! {
                _ = coordinator.cancel.cancelled() => return Err("cancelled".into()),
                changed = pause.changed() => { if changed.is_err() { break; } }
            }
        }
        write_frame(&mut stream, &ClientFrame::RequestFile).await?;
        let header: ServerFrame = timeout(Duration::from_secs(60), read_frame(&mut stream))
            .await
            .map_err(|_| "file stream header timed out".to_string())??;
        match header {
            ServerFrame::FileData {
                size: offered_size,
                sha256,
            } if offered_size == size && sha256 == offer.file_id => {}
            ServerFrame::Error { message, .. } => return Err(message),
            _ => return Err("peer returned an invalid file stream header".into()),
        }

        let mut writer = BufWriter::new(
            File::create(&partial)
                .await
                .map_err(|error| error.to_string())?,
        );
        let mut full_hasher = Sha256::new();
        let mut received = 0u64;
        let mut buffer = vec![0u8; 256 * 1024];
        let started = std::time::Instant::now();
        let mut last_update = std::time::Instant::now();
        while received < size {
            while *pause.borrow() {
                let progress = received as f64 / size.max(1) as f64 * 100.0;
                update_download(
                    db_path,
                    &offer.request_id,
                    progress,
                    "Paused",
                    "—",
                    Some(&offer.onion),
                )?;
                tokio::select! {
                    _ = coordinator.cancel.cancelled() => return Err("cancelled".into()),
                    changed = pause.changed() => { if changed.is_err() { break; } }
                }
            }
            if coordinator.cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            let wanted = (size - received).min(buffer.len() as u64) as usize;
            let count = timeout(Duration::from_secs(45), stream.read(&mut buffer[..wanted]))
                .await
                .map_err(|_| "file stream stalled".to_string())?
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("seeder closed the file stream early".into());
            }
            writer
                .write_all(&buffer[..count])
                .await
                .map_err(|error| error.to_string())?;
            full_hasher.update(&buffer[..count]);
            received += count as u64;
            if last_update.elapsed() >= Duration::from_millis(250) || received == size {
                let progress = received as f64 / size.max(1) as f64 * 100.0;
                let speed =
                    format_speed(received as f64 / started.elapsed().as_secs_f64().max(0.001));
                update_download(
                    db_path,
                    &offer.request_id,
                    progress,
                    "Downloading from fastest responsive Tor seeder",
                    &speed,
                    Some(&offer.onion),
                )?;
                last_update = std::time::Instant::now();
            }
        }
        writer.flush().await.map_err(|error| error.to_string())?;
        drop(writer);
        if hex::encode(full_hasher.finalize()) != offer.file_id {
            return Err("downloaded file SHA-256 verification failed".into());
        }
        let blocked: bool = crate::open_connection(db_path)?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
                [&offer.file_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if blocked {
            let _ = fs::remove_file(&partial).await;
            return Err("download rejected because this file hash is blocked".into());
        }
        let audio = match crate::audio::validate_audio(&partial) {
            Ok(audio) => audio,
            Err(error) => {
                let _ = fs::remove_file(&partial).await;
                return Err(format!(
                    "download rejected by the audio-only policy: {error}"
                ));
            }
        };
        let mut final_destination = destination.clone();
        loop {
            match fs::hard_link(&partial, &final_destination).await {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    final_destination = super::unique_destination(&napstr_folder, &filename);
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        fs::remove_file(&partial)
            .await
            .map_err(|error| error.to_string())?;
        *coordinator.destination.lock().await = Some(final_destination.clone());

        {
            let mut connection = crate::open_connection(db_path)?;
            let transaction = connection
                .transaction()
                .map_err(|error| error.to_string())?;
            crate::upsert_verified_file(
                &transaction,
                &napstr_folder,
                &final_destination,
                &offer.file_id,
                size,
                audio,
            )?;
            transaction
                .execute(
                    "DELETE FROM download_sources WHERE request_id=?1",
                    [&offer.request_id],
                )
                .map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "DELETE FROM network_downloads WHERE request_id=?1",
                    [&offer.request_id],
                )
                .map_err(|error| error.to_string())?;
            transaction.commit().map_err(|error| error.to_string())?;
        }
        coordinator.complete.store(true, Ordering::SeqCst);
        coordinator.cancel.cancel();

        if write_frame(&mut stream, &ClientFrame::TransferComplete)
            .await
            .is_ok()
        {
            let _ = timeout(
                Duration::from_secs(10),
                read_frame::<_, ServerFrame>(&mut stream),
            )
            .await;
        }
        Ok(())
    }
    .await;
    if final_result.is_err() {
        let _ = write_frame(&mut stream, &ClientFrame::Cancel).await;
        let _ = fs::remove_file(&partial).await;
    }
    final_result
}

fn current_progress(db_path: &Path, request_id: &str) -> Result<f64, String> {
    crate::open_connection(db_path)?
        .query_row(
            "SELECT progress FROM network_downloads WHERE request_id=?1",
            [request_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn update_source_status(
    db_path: &Path,
    request_id: &str,
    source_pubkey: &str,
    status: &str,
) -> Result<(), String> {
    crate::open_connection(db_path)?
        .execute(
            "UPDATE download_sources SET status=?1,updated_at=?2 WHERE request_id=?3 AND source_pubkey=?4",
            params![status, Utc::now().to_rfc3339(), request_id, source_pubkey],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn update_download(
    db_path: &Path,
    request_id: &str,
    progress: f64,
    status: &str,
    speed: &str,
    onion: Option<&str>,
) -> Result<(), String> {
    crate::open_connection(db_path)?.execute(
        "UPDATE network_downloads SET progress=?1, status=?2, speed=?3, onion=COALESCE(?4,onion), updated_at=?5 WHERE request_id=?6",
        params![progress, status, speed, onion, Utc::now().to_rfc3339(), request_id],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

fn record_delivery(db_path: &Path, file_id: &str) -> Result<(), String> {
    let connection = crate::open_connection(db_path)?;
    connection
        .execute(
            "INSERT INTO upload_stats(file_id, delivered, last_delivered_at) VALUES (?1, 1, ?2)
             ON CONFLICT(file_id) DO UPDATE
             SET delivered = delivered + 1, last_delivered_at = excluded.last_delivered_at",
            params![file_id, Utc::now().to_rfc3339()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn format_speed(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_second / 1024.0 / 1024.0)
    } else {
        format!("{:.0} KB/s", bytes_per_second / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tokio::io::{duplex, DuplexStream};

    fn test_peer(
        db_path: PathBuf,
        grants: Arc<RwLock<HashMap<String, SessionGrant>>>,
    ) -> DuplexStream {
        let (client, server) = duplex(2 * 1024 * 1024);
        tokio::spawn(async move {
            let _ = serve_connection(server, &db_path, grants).await;
        });
        client
    }

    fn shared_fixture() -> (PathBuf, PathBuf, String, Vec<u8>) {
        let directory =
            std::env::temp_dir().join(format!("napstr-transfer-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("napstr.sqlite3");
        crate::initialise_database(&db_path, &directory).unwrap();
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&44u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\x08\0\0\0audio123");
        let file_path = directory.join("payload.wav");
        std::fs::write(&file_path, &bytes).unwrap();
        let (file_id, size) = crate::hash_file(&file_path).unwrap();
        Connection::open(&db_path).unwrap().execute(
            "INSERT INTO files(file_id,filename,path,size,format,indexed_at,mime) VALUES(?1,'payload.wav',?2,?3,'WAV',?4,'audio/wav')",
            params![file_id, file_path.to_string_lossy(), size as i64, Utc::now().to_rfc3339()],
        ).unwrap();
        (directory, db_path, file_id, bytes)
    }

    #[tokio::test]
    async fn standby_is_promoted_only_after_primary_releases() {
        let coordinator = DownloadCoordinator::new(false);
        let primary = coordinator
            .primary_slot
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        assert!(timeout(
            Duration::from_millis(20),
            coordinator.primary_slot.clone().acquire_owned()
        )
        .await
        .is_err());
        drop(primary);
        let backup = timeout(
            Duration::from_secs(1),
            coordinator.primary_slot.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(backup);
    }

    #[tokio::test]
    async fn delivered_uploads_are_counted_and_reported_with_active_grants() {
        let (directory, db_path, file_id, _bytes) = shared_fixture();
        record_delivery(&db_path, &file_id).unwrap();
        record_delivery(&db_path, &file_id).unwrap();
        let service = TransferService::new(
            db_path.clone(),
            Arc::new(TorManager::new(directory.clone(), directory.clone())),
        );
        service.grants.write().await.insert(
            "grant-key".into(),
            SessionGrant {
                file_id: file_id.clone(),
                requester: "requester".into(),
                expires_at: Utc::now().timestamp() + 60,
                _onion_lease: None,
            },
        );

        let stats = service.seeding_stats().await.unwrap();

        let stat = stats.iter().find(|stat| stat.file_id == file_id).unwrap();
        assert_eq!(stat.delivered, 2);
        assert_eq!(stat.active_grants, 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn capability_authorizes_only_the_negotiated_file() {
        let (directory, db_path, file_id, bytes) = shared_fixture();
        let capability = "private-capability".to_string();
        let grants = Arc::new(RwLock::new(HashMap::from([(
            capability_key(&capability),
            SessionGrant {
                file_id: file_id.clone(),
                requester: "requester".into(),
                expires_at: Utc::now().timestamp() + 60,
                _onion_lease: None,
            },
        )])));

        let mut bad = test_peer(db_path.clone(), grants.clone());
        write_frame(
            &mut bad,
            &ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capability: "wrong".into(),
                file_id: file_id.clone(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, ServerFrame>(&mut bad).await.unwrap(), ServerFrame::Error { code, .. } if code == "UNAUTHORIZED")
        );

        let mut stream = test_peer(db_path, grants);
        write_frame(
            &mut stream,
            &ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capability,
                file_id,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut stream).await.unwrap(),
            ServerFrame::Welcome { .. }
        ));
        write_frame(&mut stream, &ClientFrame::RequestFile)
            .await
            .unwrap();
        let size = match read_frame::<_, ServerFrame>(&mut stream).await.unwrap() {
            ServerFrame::FileData { size, .. } => size,
            other => panic!("unexpected response: {other:?}"),
        };
        let mut received = vec![0; size as usize];
        stream.read_exact(&mut received).await.unwrap();
        assert_eq!(received, bytes);
        write_frame(&mut stream, &ClientFrame::TransferComplete)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut stream).await.unwrap(),
            ServerFrame::TransferComplete
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
