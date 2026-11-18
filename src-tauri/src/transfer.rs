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
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter, SeekFrom},
    net::TcpListener,
    sync::{watch, Mutex, Notify, RwLock, Semaphore},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

struct TemporaryPath {
    path: PathBuf,
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn is_download_temporary_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(".napstr-download-")
        .and_then(|value| value.strip_suffix(".part"))
    else {
        return false;
    };
    let (id, extension) = body.split_once('.').unwrap_or((body, ""));
    id.len() == 36
        && uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Run before starting workers/watchers. Only Napstr's temporary downloads in
/// its actual download destinations are eligible; never follow child symlinks.
pub fn cleanup_abandoned_downloads(folder: &Path) -> Result<(), String> {
    fn clean(directory: &Path) -> Result<(), String> {
        for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_download_temporary_name)
            {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }
    if !folder.exists() {
        return Ok(());
    }
    let root = std::fs::canonicalize(folder).map_err(|error| error.to_string())?;
    clean(&root)?;
    let books = root.join("Audiobooks");
    match std::fs::symlink_metadata(&books) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            for entry in std::fs::read_dir(&books).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                if entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir()
                {
                    clean(&entry.path())?;
                }
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

async fn create_temporary_file(
    directory: &Path,
    prefix: &str,
    source_name: &Path,
) -> Result<(File, TemporaryPath), String> {
    let directory = directory.to_owned();
    let prefix = prefix.to_owned();
    let extension = source_name
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    // Keep creation and ownership in the same blocking task. If the caller is
    // cancelled during open, dropping the task's result still removes the file.
    let (file, guard) = tokio::task::spawn_blocking(move || {
        for _ in 0..16 {
            let path = directory.join(format!(
                ".{prefix}-{}{extension}.part",
                uuid::Uuid::new_v4()
            ));
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((file, TemporaryPath { path })),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("could not create a private transfer file".to_string())
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok((File::from_std(file), guard))
}

fn ensure_secure_child_directory(parent: &Path, name: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let candidate = parent.join(name);
    match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("download destination is not a real directory".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&candidate).map_err(|error| error.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    }
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| error.to_string())?;
    if canonical.parent() != Some(parent) {
        return Err("download destination escapes the Napstr folder".into());
    }
    Ok(canonical)
}

fn secure_destination_root(
    napstr_folder: &Path,
    destination_folder: &str,
) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(napstr_folder).map_err(|error| error.to_string())?;
    if !root.is_dir() {
        return Err("Napstr folder is not a directory".into());
    }
    let component = Path::new(destination_folder);
    if destination_folder.is_empty()
        || component.is_absolute()
        || component.components().count() != 1
        || !component
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Ok(root);
    }
    let audiobooks = ensure_secure_child_directory(&root, std::ffi::OsStr::new("Audiobooks"))?;
    let name = component
        .file_name()
        .ok_or("invalid audiobook destination")?;
    ensure_secure_child_directory(&audiobooks, name)
}

async fn verified_shared_snapshot(
    db_path: &Path,
    path: &Path,
    expected_size: u64,
    expected_hash: &str,
) -> Result<(File, TemporaryPath), String> {
    let temp_root = db_path
        .parent()
        .ok_or("database has no private application directory")?;
    let temp_root = std::fs::canonicalize(temp_root).map_err(|error| error.to_string())?;
    let temp_root =
        ensure_secure_child_directory(&temp_root, std::ffi::OsStr::new("transfer-temp"))?;
    #[cfg(unix)]
    std::fs::set_permissions(&temp_root, {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o700)
    })
    .map_err(|error| error.to_string())?;
    let mut source = File::open(path).await.map_err(|error| error.to_string())?;
    let (mut snapshot, guard) = create_temporary_file(&temp_root, "napstr-serve", path).await?;
    let mut remaining = expected_size;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let count = source
            .read(&mut buffer[..wanted])
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("local shared file changed after indexing".into());
        }
        hasher.update(&buffer[..count]);
        snapshot
            .write_all(&buffer[..count])
            .await
            .map_err(|error| error.to_string())?;
        remaining -= count as u64;
    }
    if source
        .read(&mut buffer[..1])
        .await
        .map_err(|error| error.to_string())?
        != 0
        || hex::encode(hasher.finalize()) != expected_hash
    {
        return Err("local shared file changed after indexing".into());
    }
    snapshot.flush().await.map_err(|error| error.to_string())?;
    crate::audio::validate_audio(&guard.path)
        .map_err(|error| format!("shared file failed the audio-only policy: {error}"))?;
    snapshot
        .seek(SeekFrom::Start(0))
        .await
        .map_err(|error| error.to_string())?;
    Ok((snapshot, guard))
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

fn register_offer(
    grants: &mut HashMap<String, SessionGrant>,
    onion_lease: Arc<OnionLease>,
    request_id: String,
    file_id: String,
    requester: String,
) -> Result<DownloadOffer, String> {
    let now = Utc::now().timestamp();
    // Old services cannot accept these offers after Tor restarts.
    grants.retain(|_, grant| {
        grant.expires_at > now
            && grant._onion_lease.as_ref().is_some_and(|lease| Arc::ptr_eq(lease, &onion_lease))
    });
    if grants.len() >= 64 {
        return Err("this peer is already serving the maximum number of private offers".into());
    }
    if grants.values().any(|grant| grant.file_id == file_id && grant.requester == requester) {
        return Err("an offer for this requester and file is already active".into());
    }
    let mut random = [0u8; 32];
    rand::rng().fill_bytes(&mut random);
    let capability = hex::encode(random);
    let expires_at = now + 15 * 60;
    grants.insert(
        capability_key(&capability),
        SessionGrant {
            file_id: file_id.clone(),
            requester,
            expires_at,
            _onion_lease: Some(onion_lease.clone()),
        },
    );
    Ok(DownloadOffer {
        request_id,
        file_id,
        onion: onion_lease.onion.clone(),
        port: 80,
        capability,
        expires_at,
    })
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
    stopped: Notify,
    partial_paths: std::sync::Mutex<Vec<PathBuf>>,
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
            stopped: Notify::new(),
            partial_paths: std::sync::Mutex::new(Vec::new()),
        })
    }
}

async fn until_cancelled(
    cancel: &CancellationToken,
    transfer: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = transfer => result,
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

    pub async fn warm_for_sharing(&self) -> Result<(), String> {
        let mut session_onion = self.session_onion.lock().await;
        self.sharing_onion(&mut session_onion).await.map(|_| ())
    }

    async fn sharing_onion(
        &self,
        session_onion: &mut Option<Arc<OnionLease>>,
    ) -> Result<Arc<OnionLease>, String> {
        let port = self.ensure_listener().await?;
        let lease = self.tor.ensure_onion(port, session_onion.as_ref()).await?;
        *session_onion = Some(lease.clone());
        Ok(lease)
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
        // Keep service replacement and offer registration in the same critical
        // section: an older request must never prune a newer service's grants.
        let mut session_onion = self.session_onion.lock().await;
        let onion_lease = self.sharing_onion(&mut session_onion).await?;
        let offer = register_offer(
            &mut *self.grants.write().await,
            onion_lease,
            request_id,
            file_id,
            requester,
        )?;
        drop(session_onion);
        let grants = self.grants.clone();
        let expiring_key = capability_key(&offer.capability);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15 * 60 + 1)).await;
            grants.write().await.remove(&expiring_key);
        });
        Ok(offer)
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
        // Registration and removal share this lock so a late offer cannot
        // start another worker after its entry has been removed.
        let mut active_guard = self.active.lock().await;
        let connection = crate::open_connection(&self.db_path)?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM network_downloads WHERE request_id=?1",
                [&offer.request_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if !status
            .as_deref()
            .is_some_and(crate::network::download_accepts_offers)
        {
            return Ok(());
        }
        transaction.execute("UPDATE download_sources SET status='Connected',updated_at=?1 WHERE request_id=?2 AND source_pubkey=?3", params![Utc::now().to_rfc3339(), offer.request_id, source_pubkey]).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        let db_path = self.db_path.clone();
        let tor = self.tor.clone();
        let active = self.active.clone();
        let request_id = offer.request_id.clone();
        let coordinator = {
            let initially_paused = self.globally_paused.load(Ordering::SeqCst);
            active_guard
                .entry(request_id.clone())
                .or_insert_with(|| DownloadCoordinator::new(initially_paused))
                .clone()
        };
        coordinator.workers.fetch_add(1, Ordering::SeqCst);
        drop(active_guard);
        let pause_rx = coordinator.paused.subscribe();
        tokio::spawn(async move {
            let result = until_cancelled(
                &coordinator.cancel,
                download_offer(
                    &db_path,
                    tor,
                    &offer,
                    &source_pubkey,
                    coordinator.clone(),
                    pause_rx,
                ),
            )
            .await;
            // Serialize the final worker's status/removal with new offers so a
            // late fallback cannot revive a failed song outside the queue limit.
            let mut active_guard = active.lock().await;
            let remaining = coordinator.workers.fetch_sub(1, Ordering::SeqCst) - 1;
            let source_status = match &result {
                Ok(_) => "Complete".to_string(),
                Err(error) => format!("Failed: {error}"),
            };
            let _ = (|| -> Result<(), String> {
                let mut connection = crate::open_connection(&db_path)?;
                let transaction = connection
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| error.to_string())?;
                transaction.execute("UPDATE download_sources SET status=?1,updated_at=?2 WHERE request_id=?3 AND source_pubkey=?4", params![source_status, Utc::now().to_rfc3339(), offer.request_id, source_pubkey]).map_err(|error| error.to_string())?;
                if let Err(error) = result {
                    if remaining == 0 && !coordinator.complete.load(Ordering::SeqCst) {
                        let awaiting_offer: bool = transaction.query_row(
                            "SELECT EXISTS(SELECT 1 FROM download_sources WHERE request_id=?1 AND status='Requested')",
                            [&offer.request_id], |row| row.get(0),
                        ).map_err(|error| error.to_string())?;
                        let status = if coordinator.cancel.is_cancelled() {
                            "Cancelled".to_string()
                        } else if awaiting_offer {
                            crate::network::DOWNLOAD_WAITING_FALLBACK.to_string()
                        } else {
                            format!("Failed: {error}")
                        };
                        transaction.execute(
                            "UPDATE network_downloads SET status=?1,speed='—',updated_at=?2 WHERE request_id=?3",
                            params![status, Utc::now().to_rfc3339(), offer.request_id],
                        ).map_err(|error| error.to_string())?;
                    }
                }
                transaction.commit().map_err(|error| error.to_string())
            })();
            if remaining == 0 {
                active_guard.remove(&request_id);
                coordinator.stopped.notify_waiters();
            }
        });
        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        self.globally_paused.load(Ordering::SeqCst)
    }

    pub async fn set_paused(&self, paused: bool) {
        self.globally_paused.store(paused, Ordering::SeqCst);
        for transfer in self.active.lock().await.values() {
            let _ = transfer.paused.send(paused);
        }
    }

    pub async fn remove_downloads(&self, rowid: Option<i64>) -> Result<(), String> {
        let stopped = {
            let active = self.active.lock().await;
            let mut connection = crate::open_connection(&self.db_path)?;
            let transaction = connection
                .transaction()
                .map_err(|error| error.to_string())?;
            let requests = {
                let mut statement = transaction
                    .prepare(
                        "SELECT request_id FROM network_downloads WHERE ?1 IS NULL OR rowid=?1",
                    )
                    .map_err(|error| error.to_string())?;
                let rows = statement
                    .query_map([rowid], |row| row.get::<_, String>(0))
                    .map_err(|error| error.to_string())?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            };
            for request in &requests {
                transaction
                    .execute(
                        "DELETE FROM download_sources WHERE request_id=?1",
                        [request],
                    )
                    .map_err(|error| error.to_string())?;
                transaction
                    .execute(
                        "DELETE FROM network_downloads WHERE request_id=?1",
                        [request],
                    )
                    .map_err(|error| error.to_string())?;
            }
            transaction.commit().map_err(|error| error.to_string())?;
            let stopped = requests
                .iter()
                .filter_map(|request| active.get(request).cloned())
                .collect::<Vec<_>>();
            for transfer in &stopped {
                transfer.cancel.cancel();
            }
            stopped
        };
        for transfer in stopped {
            loop {
                let notified = transfer.stopped.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if transfer.workers.load(Ordering::SeqCst) == 0 {
                    break;
                }
                notified.await;
            }
            // Retry cleanup after all file handles have been dropped. These
            // paths belong to this request, never to another running instance.
            let paths = transfer
                .partial_paths
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            for path in paths {
                match fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not remove partial download {}: {error}",
                            path.display()
                        ))
                    }
                }
            }
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
    let size = u64::try_from(size).map_err(|_| "shared file has an invalid size")?;
    let (mut file, _snapshot) =
        match verified_shared_snapshot(db_path, Path::new(&path), size, &file_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                write_frame(
                    &mut stream,
                    &ServerFrame::Error {
                        code: "LOCAL_FILE_UNAVAILABLE".into(),
                        message: error.clone(),
                    },
                )
                .await?;
                return Err(error);
            }
        };
    write_frame(
        &mut stream,
        &ServerFrame::Welcome {
            version: PROTOCOL_VERSION,
            file_id: file_id.clone(),
            filename,
            size,
        },
    )
    .await?;

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
                        size,
                        sha256: file_id.clone(),
                    },
                )
                .await?;
                let mut remaining = size;
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
                    stream
                        .write_all(&buffer[..count])
                        .await
                        .map_err(|error| error.to_string())?;
                    remaining -= count as u64;
                }
                stream.flush().await.map_err(|error| error.to_string())?;
                sent_file = true;
            }
            ClientFrame::RequestFile => return Err("duplicate file request".into()),
            ClientFrame::TransferComplete => {
                write_frame(&mut stream, &ServerFrame::TransferComplete).await?;
                grants.write().await.remove(&key);
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
    let (filename, expected_size, destination_folder): (String, i64, String) = crate::open_connection(db_path)?
        .query_row(
            "SELECT filename,size,destination_folder FROM network_downloads WHERE request_id=?1 AND file_id=?2",
            params![offer.request_id, offer.file_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
    let destination_root = secure_destination_root(&napstr_folder, &destination_folder)?;
    let destination = {
        let mut selected = coordinator.destination.lock().await;
        selected
            .get_or_insert_with(|| super::unique_destination(&destination_root, &filename))
            .clone()
    };
    drop(connection);
    let (partial_file, partial_guard) =
        create_temporary_file(&destination_root, "napstr-download", Path::new(&filename)).await?;
    let partial = partial_guard.path.clone();
    coordinator
        .partial_paths
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(partial.clone());
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

        let mut writer = BufWriter::new(partial_file);
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
        let current_destination_root =
            secure_destination_root(&napstr_folder, &destination_folder)?;
        if current_destination_root != destination_root {
            return Err("download destination changed during transfer".into());
        }
        loop {
            match fs::hard_link(&partial, &final_destination).await {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    final_destination = super::unique_destination(&destination_root, &filename);
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
    drop(partial_guard);
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

    #[tokio::test]
    async fn service_replacement_releases_old_offers_but_preserves_current_limits() {
        let control = crate::tor::tests::TestControl::start().await;
        let first = control.lease(uuid::Uuid::new_v4(), None).await.unwrap();
        let replacement = control.lease(uuid::Uuid::new_v4(), Some(&first)).await.unwrap();
        let mut grants = HashMap::new();
        let issue = |grants: &mut HashMap<String, SessionGrant>, lease, file: &str| {
            register_offer(grants, lease, "request".into(), file.into(), "requester".into())
        };
        let old = issue(&mut grants, first.clone(), "song").unwrap();
        assert!(issue(&mut grants, first, "song").is_err());
        let fresh = issue(&mut grants, replacement.clone(), "song").unwrap();
        assert_eq!(grants.len(), 1);
        assert!(!grants.contains_key(&capability_key(&old.capability)));
        assert!(grants.contains_key(&capability_key(&fresh.capability)));
        assert!(issue(&mut grants, replacement.clone(), "song").is_err());
        for index in 1..64 {
            issue(&mut grants, replacement.clone(), &format!("song-{index}")).unwrap();
        }
        assert!(issue(&mut grants, replacement.clone(), "overflow").is_err());
        grants.get_mut(&capability_key(&fresh.capability)).unwrap().expires_at = 0;
        assert!(issue(&mut grants, replacement, "song").is_ok());
        assert_eq!(grants.len(), 64);
    }

    #[test]
    fn cancellation_during_file_creation_cleans_up() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory =
                std::env::temp_dir().join(format!("napstr-cancel-race-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let (release, wait) = std::sync::mpsc::channel();
            let (started, ready) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            ready.await.unwrap();
            let token = CancellationToken::new();
            let child_token = token.clone();
            let target = directory.clone();
            let worker = tokio::spawn(async move {
                until_cancelled(&child_token, async move {
                    let (_file, _guard) =
                        create_temporary_file(&target, "napstr-download", Path::new("song.mp3"))
                            .await?;
                    std::future::pending::<Result<(), String>>().await
                })
                .await
            });
            tokio::task::yield_now().await;
            token.cancel();
            assert_eq!(worker.await.unwrap().unwrap_err(), "cancelled");
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::task::spawn_blocking(|| ()).await.unwrap();
            let paths = std::fs::read_dir(&directory)
                .unwrap()
                .map(|x| x.unwrap().path())
                .collect::<Vec<_>>();
            assert!(
                paths.is_empty(),
                "cancelled creation left an orphan: {paths:?}"
            );
            std::fs::remove_dir_all(&directory).unwrap();
        });
    }

    #[tokio::test]
    async fn queued_and_finished_downloads_ignore_late_offers_and_queued_cancel_is_immediate() {
        let (directory, db_path, file_id, _) = shared_fixture();
        let service = TransferService::new(
            db_path.clone(),
            Arc::new(TorManager::new(directory.clone(), directory.clone())),
        );
        let connection = crate::open_connection(&db_path).unwrap();
        for status in [
            "Queued",
            "Waiting to restart after reconnect",
            "Failed: no seeder responded",
            "Cancelled",
            "All seeders refused",
            "Verified · Complete",
        ] {
            connection.execute(
                "INSERT INTO network_downloads(request_id,file_id,source_pubkey,filename,size,progress,status,speed,destination,onion,updated_at) VALUES(?1,?2,'source','song.wav',100,0,?1,'Queued','','','now')",
                params![status, file_id],
            ).unwrap();
            connection
                .execute(
                    "INSERT INTO download_sources VALUES(?1,'source','Queued','now')",
                    [status],
                )
                .unwrap();
            service
                .accept_offer(
                    DownloadOffer {
                        request_id: status.into(),
                        file_id: file_id.clone(),
                        onion: format!("{}.onion", "a".repeat(56)),
                        port: 80,
                        capability: "late-offer".into(),
                        expires_at: Utc::now().timestamp() + 60,
                    },
                    "source".into(),
                )
                .await
                .unwrap();
        }
        assert!(service.active.lock().await.is_empty());
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM download_sources WHERE status='Connected'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        service.set_paused(true).await;
        assert!(service.is_paused());
        let rowid = connection
            .query_row(
                "SELECT rowid FROM network_downloads WHERE request_id='Queued'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        timeout(
            Duration::from_secs(1),
            service.remove_downloads(Some(rowid)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM download_sources WHERE request_id='Queued'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        service.set_paused(false).await;
        assert!(!service.is_paused());
        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn removing_downloads_stops_stalled_workers_and_preserves_completed_and_unrelated_files()
    {
        let (directory, db_path, file_id, bytes) = shared_fixture();
        let service = TransferService::new(
            db_path.clone(),
            Arc::new(TorManager::new(directory.clone(), directory.clone())),
        );
        let connection = crate::open_connection(&db_path).unwrap();
        for request in ["active", "waiting", "failed"] {
            connection.execute(
                "INSERT INTO network_downloads(request_id,file_id,source_pubkey,filename,size,progress,status,speed,destination,onion,updated_at)
                 VALUES(?1,?2,'source','song.wav',100,12,?3,'—','','','now')",
                params![request, file_id, if request == "failed" { "Failed: stalled" } else { "Connecting" }],
            ).unwrap();
            connection
                .execute(
                    "INSERT INTO download_sources VALUES(?1,'source','Requested','now')",
                    [request],
                )
                .unwrap();
        }
        let rowid: i64 = connection
            .query_row(
                "SELECT rowid FROM network_downloads WHERE request_id='active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let coordinator = DownloadCoordinator::new(true);
        coordinator.workers.store(1, Ordering::SeqCst);
        let (file, guard) =
            create_temporary_file(&directory, "napstr-download", Path::new("song.wav"))
                .await
                .unwrap();
        let partial = guard.path.clone();
        coordinator
            .partial_paths
            .lock()
            .unwrap()
            .push(partial.clone());
        service
            .active
            .lock()
            .await
            .insert("active".into(), coordinator.clone());
        let unrelated = directory.join(format!(
            ".napstr-download-{}.wav.part",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&unrelated, b"another instance").unwrap();
        let active = service.active.clone();
        let worker = tokio::spawn(async move {
            let result = until_cancelled(&coordinator.cancel, async move {
                let _file = file;
                let _guard = guard;
                std::future::pending::<Result<(), String>>().await
            })
            .await;
            assert_eq!(result.unwrap_err(), "cancelled");
            coordinator.workers.store(0, Ordering::SeqCst);
            active.lock().await.remove("active");
            coordinator.stopped.notify_waiters();
        });
        timeout(
            Duration::from_secs(1),
            service.remove_downloads(Some(rowid)),
        )
        .await
        .unwrap()
        .unwrap();
        worker.await.unwrap();
        assert!(!partial.exists());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"another instance");
        assert_eq!(std::fs::read(directory.join("payload.wav")).unwrap(), bytes);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM network_downloads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        service.remove_downloads(None).await.unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM network_downloads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM download_sources", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM files", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        service
            .accept_offer(
                DownloadOffer {
                    request_id: "active".into(),
                    file_id,
                    onion: format!("{}.onion", "a".repeat(56)),
                    port: 80,
                    capability: "expired-request".into(),
                    expires_at: Utc::now().timestamp() + 60,
                },
                "source".into(),
            )
            .await
            .unwrap();
        assert!(service.active.lock().await.is_empty());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"another instance");
        drop(connection);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn startup_cleanup_only_removes_napstr_download_partials() {
        let directory =
            std::env::temp_dir().join(format!("napstr-cleanup-test-{}", uuid::Uuid::new_v4()));
        let root = directory.join("downloads");
        let book = root.join("Audiobooks/Book");
        let outside = directory.join("outside");
        std::fs::create_dir_all(&book).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let mut abandoned = Vec::new();
        for destination in [&root, &book] {
            let (file, guard) =
                create_temporary_file(destination, "napstr-download", Path::new("song.flac"))
                    .await
                    .unwrap();
            abandoned.push(guard.path.clone());
            drop(file);
            std::mem::forget(guard); // Simulate process exit without destructors.
        }
        let mut retained = Vec::new();
        for name in [
            "song.flac",
            "other.part",
            ".napstr-download-not-a-uuid.flac.part",
            ".napstr-serve-123.part",
        ] {
            let path = root.join(name);
            std::fs::write(&path, b"keep").unwrap();
            retained.push(path);
        }
        let outside_partial = outside.join(format!(
            ".napstr-download-{}.mp3.part",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&outside_partial, b"keep").unwrap();
        retained.push(outside_partial.clone());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("Audiobooks/Linked")).unwrap();
            let link = root.join(format!(
                ".napstr-download-{}.mp3.part",
                uuid::Uuid::new_v4()
            ));
            std::os::unix::fs::symlink(&outside_partial, &link).unwrap();
            retained.push(link);
        }
        cleanup_abandoned_downloads(&root).unwrap();
        cleanup_abandoned_downloads(&root).unwrap();
        assert!(abandoned.iter().all(|path| !path.exists()));
        for path in retained {
            assert_eq!(std::fs::read(&path).unwrap(), b"keep", "{}", path.display());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

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

    #[tokio::test]
    async fn changed_shared_file_is_rejected_before_any_bytes_are_offered() {
        let (directory, db_path, file_id, mut bytes) = shared_fixture();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(directory.join("payload.wav"), bytes).unwrap();
        let capability = "changed-file-capability".to_string();
        let grants = Arc::new(RwLock::new(HashMap::from([(
            capability_key(&capability),
            SessionGrant {
                file_id: file_id.clone(),
                requester: "requester".into(),
                expires_at: Utc::now().timestamp() + 60,
                _onion_lease: None,
            },
        )])));
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
            ServerFrame::Error { code, .. } if code == "LOCAL_FILE_UNAVAILABLE"
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn audiobook_download_rejects_a_symlinked_destination() {
        use std::os::unix::fs::symlink;

        let directory =
            std::env::temp_dir().join(format!("napstr-destination-test-{}", uuid::Uuid::new_v4()));
        let shared = directory.join("shared");
        let outside = directory.join("outside");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, shared.join("Audiobooks")).unwrap();

        assert!(secure_destination_root(&shared, "A Book").is_err());
        assert!(!outside.join("A Book").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
