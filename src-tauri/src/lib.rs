use chrono::Utc;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Read, Seek},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::UNIX_EPOCH,
};
use tauri::{Emitter, Manager, State};
use walkdir::WalkDir;

mod audio;
mod network;
mod player;
mod protocol;
mod tor;
mod transfer;

const HASH_BUFFER_SIZE: usize = 256 * 1024;
const MAX_INDEX_ERRORS: usize = 100;
const INDEX_PROGRESS_INTERVAL: usize = 25;
const INDEX_COMMIT_BATCH_SIZE: usize = 50;
const LIBRARY_CHANGED_EVENT: &str = "napstr-library-changed";
const INDEX_BATCH_EVENT: &str = "napstr-index-batch";
const INDEX_PROGRESS_EVENT: &str = "napstr-index-progress";
const DEFAULT_NOSTR_RELAYS: &str = "wss://relay.damus.io,wss://nos.lol,wss://relay.nostr.com,wss://relay.primal.net,wss://relay.snort.social,wss://nostr.mom";
const LEGACY_DEFAULT_NOSTR_RELAYS: &str = "wss://relay.damus.io,wss://nos.lol";

struct AppState {
    db_path: Mutex<PathBuf>,
    network: Arc<network::NetworkService>,
    tor: Arc<tor::TorManager>,
    watcher: Mutex<Option<FolderWatcher>>,
    scan_lock: Arc<Mutex<()>>,
    scan_cancel: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
    player: Arc<player::NativePlayer>,
    recovering_after_sleep: Arc<AtomicBool>,
}

struct ShutdownServices {
    network: Arc<network::NetworkService>,
    tor: Arc<tor::TorManager>,
}

struct FolderWatcher {
    _watcher: RecommendedWatcher,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedFile {
    file_id: String,
    filename: String,
    path: String,
    folder: String,
    size: u64,
    format: String,
    status: String,
    title: String,
    artist: String,
    album: String,
    mime: String,
    license: String,
    description: String,
    tags: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Transfer {
    id: i64,
    file_id: String,
    filename: String,
    size: u64,
    progress: f64,
    status: String,
    speed: String,
    destination: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    napstr_folder: String,
    nostr_relays: String,
    display_name: String,
    profile_about: String,
    profile_picture: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSnapshot {
    files: Vec<SharedFile>,
    transfers: Vec<Transfer>,
    settings: Settings,
    indexed_bytes: u64,
    native: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexReport {
    file_count: usize,
    total_bytes: u64,
    errors: Vec<String>,
    error_count: usize,
    changed_files: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexProgress {
    scanning: bool,
    processed_files: usize,
    indexed_files: usize,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexBatch {
    files: Vec<SharedFile>,
    file_count: usize,
    total_bytes: u64,
}

type VerifiedIndexFile = (PathBuf, Option<audio::AudioInfo>, String, u64);

fn open_db(state: &State<'_, AppState>) -> Result<Connection, String> {
    let path = state
        .db_path
        .lock()
        .map_err(|_| "database lock poisoned")?
        .clone();
    open_connection(&path)
}

fn open_connection(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(15))
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn initialise_database(path: &Path, app_data: &Path) -> Result<(), String> {
    fs::create_dir_all(app_data).map_err(|error| error.to_string())?;
    let connection = open_connection(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS files (
           file_id TEXT PRIMARY KEY,
           filename TEXT NOT NULL,
           path TEXT NOT NULL,
           size INTEGER NOT NULL,
           format TEXT NOT NULL,
           indexed_at TEXT NOT NULL,
           title TEXT NOT NULL DEFAULT '', artist TEXT NOT NULL DEFAULT '', album TEXT NOT NULL DEFAULT '',
           mime TEXT NOT NULL DEFAULT 'application/octet-stream', license TEXT NOT NULL DEFAULT 'unspecified',
           description TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '', folder TEXT NOT NULL DEFAULT '',
           modified_ns INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS transfers (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           file_id TEXT NOT NULL,
           filename TEXT NOT NULL,
           size INTEGER NOT NULL,
           progress REAL NOT NULL DEFAULT 0,
           status TEXT NOT NULL,
           speed TEXT NOT NULL DEFAULT '—',
           destination TEXT NOT NULL DEFAULT '',
           created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS blocked_files (
           file_id TEXT PRIMARY KEY, reason TEXT NOT NULL, created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS blocked_pubkeys (
           pubkey TEXT PRIMARY KEY, reason TEXT NOT NULL, created_at TEXT NOT NULL
         );
         DROP TABLE IF EXISTS download_chunks;"
    ).map_err(|error| error.to_string())?;
    // Pre-release databases used these transfer fields in the library table.
    // Whole-file streaming no longer needs them.
    let _ = connection.execute("ALTER TABLE files DROP COLUMN chunk_hashes", []);
    let _ = connection.execute("ALTER TABLE files DROP COLUMN chunk_size", []);
    for (column, declaration) in [
        ("title", "TEXT NOT NULL DEFAULT ''"),
        ("artist", "TEXT NOT NULL DEFAULT ''"),
        ("album", "TEXT NOT NULL DEFAULT ''"),
        ("mime", "TEXT NOT NULL DEFAULT 'application/octet-stream'"),
        ("license", "TEXT NOT NULL DEFAULT 'unspecified'"),
        ("description", "TEXT NOT NULL DEFAULT ''"),
        ("tags", "TEXT NOT NULL DEFAULT ''"),
        ("folder", "TEXT NOT NULL DEFAULT ''"),
        ("modified_ns", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        ensure_column(&connection, "files", column, declaration)?;
    }
    network::initialise_network_schema(&connection)?;
    connection
        .execute_batch(
            "DELETE FROM download_sources WHERE request_id IN (
               SELECT request_id FROM network_downloads WHERE status='Verified · Complete'
             );
             DELETE FROM network_downloads WHERE status='Verified · Complete';
             DELETE FROM transfers WHERE status='Verified · Complete';",
        )
        .map_err(|error| error.to_string())?;

    let downloads_path = app_data.join("Downloads");
    fs::create_dir_all(&downloads_path).map_err(|error| error.to_string())?;
    let downloads = downloads_path.to_string_lossy().into_owned();
    for (key, value) in [
        ("shared_folder", downloads.clone()),
        ("nostr_relays", DEFAULT_NOSTR_RELAYS.to_string()),
        ("display_name", "napstr-user".to_string()),
        (
            "profile_about",
            "Sharing files privately with Napstr. napstr.net".to_string(),
        ),
        ("profile_picture", "".to_string()),
        ("profile_event_fingerprint", "".to_string()),
    ] {
        connection
            .execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute("DELETE FROM settings WHERE key='download_folder'", [])
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE settings SET value=?1 WHERE key='shared_folder' AND trim(value)=''",
            [&downloads],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE settings SET value=?1 WHERE key='nostr_relays' AND replace(value,' ','')=?2",
            params![DEFAULT_NOSTR_RELAYS, LEGACY_DEFAULT_NOSTR_RELAYS],
        )
        .map_err(|error| error.to_string())?;
    let migrated_profile = connection
        .execute(
            "UPDATE settings SET value=?1 WHERE key='profile_about' AND value=?2",
            params![
                "Sharing files privately with Napstr. napstr.net",
                "Sharing files privately with Napstr."
            ],
        )
        .map_err(|error| error.to_string())?;
    if migrated_profile > 0 {
        connection
            .execute(
                "UPDATE settings SET value='' WHERE key='profile_event_fingerprint'",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn get_setting(connection: &Connection, key: &str) -> Result<String, String> {
    connection
        .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())
}

fn load_settings(connection: &Connection) -> Result<Settings, String> {
    Ok(Settings {
        napstr_folder: get_setting(connection, "shared_folder")?,
        nostr_relays: get_setting(connection, "nostr_relays")?,
        display_name: get_setting(connection, "display_name")?,
        profile_about: get_setting(connection, "profile_about")?,
        profile_picture: get_setting(connection, "profile_picture")?,
    })
}

fn library_folder(root: &Path, file: &Path) -> String {
    file.parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .map(|relative| {
            relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(value) => Some(value.to_string_lossy()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn shared_file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SharedFile> {
    let path: String = row.get(2)?;
    let filename: String = row.get(1)?;
    Ok(SharedFile {
        file_id: row.get(0)?,
        filename: filename.clone(),
        folder: row.get(6)?,
        path,
        size: row.get::<_, i64>(3)? as u64,
        format: row.get(4)?,
        status: "Published".into(),
        title: filename,
        artist: String::new(),
        album: String::new(),
        mime: row.get(5)?,
        license: "unspecified".into(),
        description: String::new(),
        tags: row.get(7)?,
    })
}

fn load_files(connection: &Connection, query: Option<&str>) -> Result<Vec<SharedFile>, String> {
    let mut statement = connection
        .prepare(
            "SELECT file_id, filename, path, size, format, mime, folder, tags FROM files
         WHERE format IN ('MP3','FLAC','WAV','OGG','OPUS')
           AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)
         ORDER BY filename",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], shared_file_from_row)
        .map_err(|error| error.to_string())?;
    let files = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let query = query.unwrap_or_default();
    Ok(files
        .into_iter()
        .filter(|file| search_matches(query, &[&file.filename, &file.tags]))
        .collect())
}

fn load_files_by_id(
    connection: &Connection,
    file_ids: &[String],
) -> Result<Vec<SharedFile>, String> {
    let mut statement = connection
        .prepare(
            "SELECT file_id, filename, path, size, format, mime, folder, tags FROM files
             WHERE file_id=?1
               AND format IN ('MP3','FLAC','WAV','OGG','OPUS')
               AND NOT EXISTS(SELECT 1 FROM blocked_files WHERE blocked_files.file_id=files.file_id)",
        )
        .map_err(|error| error.to_string())?;
    let mut files = Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        if let Some(file) = statement
            .query_row([file_id], shared_file_from_row)
            .optional()
            .map_err(|error| error.to_string())?
        {
            files.push(file);
        }
    }
    Ok(files)
}

fn search_matches(query: &str, fields: &[&str]) -> bool {
    let query_tokens = search_tokens(query);
    if query_tokens.is_empty() {
        return true;
    }
    let field_tokens = fields
        .iter()
        .flat_map(|field| search_tokens(field))
        .collect::<Vec<_>>();
    query_tokens.iter().all(|query_token| {
        field_tokens.iter().any(|field_token| {
            field_token.contains(query_token)
                || (query_token.chars().count() >= 5
                    && edit_distance_at_most(query_token, field_token, 1))
        })
    })
}

fn search_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn edit_distance_at_most(left: &str, right: &str, limit: usize) -> bool {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.len().abs_diff(right.len()) > limit {
        return false;
    }
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_character) in left.iter().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_character) in right.iter().enumerate() {
            current.push(
                (previous[right_index + 1] + 1)
                    .min(current[right_index] + 1)
                    .min(previous[right_index] + usize::from(left_character != right_character)),
            );
        }
        if current.iter().copied().min().unwrap_or(limit + 1) > limit {
            return false;
        }
        previous = current;
    }
    previous[right.len()] <= limit
}

fn load_transfers(connection: &Connection) -> Result<Vec<Transfer>, String> {
    let mut statement = connection.prepare("SELECT id, file_id, filename, size, progress, status, speed, destination FROM transfers ORDER BY id DESC").map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(Transfer {
                id: row.get(0)?,
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
    let mut transfers = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    transfers.extend(network::load_network_transfers(connection)?);
    Ok(transfers)
}

fn snapshot(connection: &Connection) -> Result<AppSnapshot, String> {
    let files = load_files(connection, None)?;
    let indexed_bytes = files.iter().map(|file| file.size).sum();
    Ok(AppSnapshot {
        files,
        transfers: load_transfers(connection)?,
        settings: load_settings(connection)?,
        indexed_bytes,
        native: true,
    })
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !columns.iter().any(|existing| existing == column) {
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn hash_open_file(file: &File) -> Result<(String, u64), String> {
    let size = file.metadata().map_err(|error| error.to_string())?.len();
    let mut reader = BufReader::new(file.try_clone().map_err(|error| error.to_string())?);
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut full_hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        full_hasher.update(&buffer[..count]);
    }
    Ok((hex::encode(full_hasher.finalize()), size))
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    hash_open_file(&file)
}

fn modified_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| {
            duration
                .as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(duration.subsec_nanos()))
                .min(i64::MAX as u64) as i64
        })
        .unwrap_or(0)
}

pub(crate) fn upsert_verified_file(
    connection: &Connection,
    folder_root: &Path,
    path: &Path,
    file_id: &str,
    size: u64,
    audio: audio::AudioInfo,
) -> Result<(), String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Unnamed file");
    let relative_folder = library_folder(folder_root, path);
    let modified = fs::metadata(path)
        .map(|metadata| modified_ns(&metadata))
        .unwrap_or(0);
    connection
        .execute(
            "INSERT INTO files (file_id, filename, path, size, format, indexed_at, mime, folder, modified_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(file_id) DO UPDATE SET filename=excluded.filename,path=excluded.path,size=excluded.size,
             format=excluded.format,mime=excluded.mime,folder=excluded.folder,indexed_at=excluded.indexed_at,
             modified_ns=excluded.modified_ns",
            params![
                file_id,
                filename,
                path.to_string_lossy(),
                size as i64,
                audio.format,
                Utc::now().to_rfc3339(),
                audio.mime,
                relative_folder,
                modified
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn supported_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp3" | "flac" | "wav" | "ogg" | "opus"
            )
        })
}

fn record_index_error(report: &mut IndexReport, message: String) {
    report.error_count += 1;
    if report.errors.len() < MAX_INDEX_ERRORS {
        report.errors.push(message);
    }
}

fn validate_and_hash_index_file(
    path: &Path,
    expected_modified_ns: i64,
) -> Result<VerifiedIndexFile, String> {
    let audio = audio::validate_audio(path)?;
    let (file_id, size) = hash_file(path)?;
    let unchanged_during_hash = fs::metadata(path)
        .map(|after| after.len() == size && modified_ns(&after) == expected_modified_ns)
        .unwrap_or(false);
    if !unchanged_during_hash {
        return Err("file changed while it was being indexed".into());
    }
    Ok((path.to_path_buf(), Some(audio), file_id, size))
}

fn commit_index_batch(
    connection: &mut Connection,
    folder: &Path,
    batch: &mut Vec<VerifiedIndexFile>,
    report: &mut IndexReport,
) -> Result<Vec<SharedFile>, String> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|error| error.to_string())?;
    let mut changed_ids = Vec::new();
    let mut changed_ids_seen = HashSet::new();
    for (path, audio, file_id, size) in batch.drain(..) {
        let blocked: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
                [&file_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if blocked {
            record_index_error(
                report,
                format!("{}: this file hash is blocked", path.display()),
            );
            continue;
        }
        if let Some(audio) = audio {
            upsert_verified_file(&transaction, folder, &path, &file_id, size, audio)?;
            report.changed_files += 1;
            if changed_ids_seen.insert(file_id.clone()) {
                changed_ids.push(file_id.clone());
            }
        }
        transaction
            .execute(
                "INSERT OR IGNORE INTO napstr_seen(file_id) VALUES (?1)",
                [&file_id],
            )
            .map_err(|error| error.to_string())?;
        report.file_count += 1;
        report.total_bytes += size;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    load_files_by_id(connection, &changed_ids)
}

fn index_path_with_progress(
    connection: &mut Connection,
    folder: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(usize, usize),
    mut batch_committed: impl FnMut(IndexBatch),
) -> Result<IndexReport, String> {
    if !folder.is_dir() {
        return Err("The selected Napstr folder does not exist or is not a directory".into());
    }
    let mut report = IndexReport {
        file_count: 0,
        total_bytes: 0,
        errors: Vec::new(),
        error_count: 0,
        changed_files: 0,
    };
    let mut verified = Vec::with_capacity(INDEX_COMMIT_BATCH_SIZE);
    let mut processed_files = 0usize;
    let existing = {
        let mut statement = connection
            .prepare("SELECT path,file_id,size,modified_ns FROM files")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, i64>(3)?,
                    ),
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<HashMap<_, _>, _>>()
            .map_err(|error| error.to_string())?
    };

    connection
        .execute_batch(
            "DROP TABLE IF EXISTS temp.napstr_seen;
             CREATE TEMP TABLE napstr_seen(file_id TEXT PRIMARY KEY);",
        )
        .map_err(|error| error.to_string())?;

    // Audio validation and SHA-256 hashing stay outside write transactions.
    // Small commits make verified tracks visible without holding the database
    // while the rest of a large collection is scanned.
    for entry in WalkDir::new(folder)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && supported_audio_path(entry.path()))
    {
        if cancelled.load(Ordering::Relaxed) {
            let _ = connection.execute("DROP TABLE IF EXISTS temp.napstr_seen", []);
            return Err("Indexing cancelled".into());
        }
        processed_files += 1;
        let path = entry.path();
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                record_index_error(&mut report, format!("{}: {error}", path.display()));
                continue;
            }
        };
        let path_key = path.to_string_lossy();
        let current_modified_ns = modified_ns(&metadata);
        let unchanged = current_modified_ns != 0
            && existing
                .get(path_key.as_ref())
                .is_some_and(|(_, size, stored_modified_ns)| {
                    *size == metadata.len() && *stored_modified_ns == current_modified_ns
                });
        if unchanged {
            let (file_id, size, _) = existing
                .get(path_key.as_ref())
                .expect("unchanged index entry disappeared");
            verified.push((path.to_path_buf(), None, file_id.clone(), *size));
        } else {
            match validate_and_hash_index_file(path, current_modified_ns) {
                Ok(file) => verified.push(file),
                Err(error) => {
                    record_index_error(&mut report, format!("{}: {}", path.display(), error))
                }
            }
        }
        if verified.len() >= INDEX_COMMIT_BATCH_SIZE {
            let files = commit_index_batch(connection, folder, &mut verified, &mut report)?;
            if !files.is_empty() {
                batch_committed(IndexBatch {
                    files,
                    file_count: report.file_count,
                    total_bytes: report.total_bytes,
                });
            }
        }
        if processed_files % INDEX_PROGRESS_INTERVAL == 0 {
            progress(processed_files, report.file_count + verified.len());
        }
    }

    if cancelled.load(Ordering::Relaxed) {
        let _ = connection.execute("DROP TABLE IF EXISTS temp.napstr_seen", []);
        return Err("Indexing cancelled".into());
    }
    let files = commit_index_batch(connection, folder, &mut verified, &mut report)?;
    if !files.is_empty() {
        batch_committed(IndexBatch {
            files,
            file_count: report.file_count,
            total_bytes: report.total_bytes,
        });
    }
    progress(processed_files, report.file_count);

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    report.changed_files += transaction
        .execute(
            "DELETE FROM files WHERE file_id NOT IN (SELECT file_id FROM napstr_seen)",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute("DROP TABLE napstr_seen", [])
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(report)
}

#[cfg(test)]
fn index_path(connection: &mut Connection, folder: &Path) -> Result<IndexReport, String> {
    index_path_with_progress(
        connection,
        folder,
        &AtomicBool::new(false),
        |_, _| {},
        |_| {},
    )
}

fn run_index_job(
    db_path: &Path,
    folder: &Path,
    scan_lock: &Mutex<()>,
    scan_cancel: &AtomicBool,
    app_handle: &tauri::AppHandle,
    network: &Arc<network::NetworkService>,
) -> Result<IndexReport, String> {
    let _scan_guard = scan_lock.lock().map_err(|_| "library scan lock poisoned")?;
    scan_cancel.store(false, Ordering::SeqCst);
    let _ = app_handle.emit(
        INDEX_PROGRESS_EVENT,
        IndexProgress {
            scanning: true,
            processed_files: 0,
            indexed_files: 0,
            message: "Scanning the Napstr folder…".into(),
        },
    );
    let mut connection = open_connection(db_path)?;
    let result = index_path_with_progress(
        &mut connection,
        folder,
        scan_cancel,
        |processed_files, indexed_files| {
            let _ = app_handle.emit(
                INDEX_PROGRESS_EVENT,
                IndexProgress {
                    scanning: true,
                    processed_files,
                    indexed_files,
                    message: format!("Checking audio files… {processed_files}"),
                },
            );
        },
        |batch| {
            let file_ids = batch
                .files
                .iter()
                .map(|file| file.file_id.clone())
                .collect::<Vec<_>>();
            let _ = app_handle.emit(INDEX_BATCH_EVENT, batch);
            network.queue_catalogue_files(file_ids);
        },
    );
    match &result {
        Ok(report) => {
            if report.changed_files > 0 {
                let _ = app_handle.emit(LIBRARY_CHANGED_EVENT, report.clone());
                network.queue_catalogue_publish(false);
            }
            let _ = app_handle.emit(
                INDEX_PROGRESS_EVENT,
                IndexProgress {
                    scanning: false,
                    processed_files: report.file_count + report.error_count,
                    indexed_files: report.file_count,
                    message: format!("Indexed {} audio file(s)", report.file_count),
                },
            );
        }
        Err(error) => {
            let _ = app_handle.emit(
                INDEX_PROGRESS_EVENT,
                IndexProgress {
                    scanning: false,
                    processed_files: 0,
                    indexed_files: 0,
                    message: error.clone(),
                },
            );
        }
    }
    result
}

fn start_folder_watcher(
    folder: PathBuf,
    db_path: PathBuf,
    network: Arc<network::NetworkService>,
    scan_lock: Arc<Mutex<()>>,
    scan_cancel: Arc<AtomicBool>,
    app_handle: tauri::AppHandle,
) -> Result<FolderWatcher, String> {
    let (event_tx, event_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    })
    .map_err(|error| error.to_string())?;
    watcher
        .watch(&folder, RecursiveMode::Recursive)
        .map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("napstr-folder-watch".into())
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                let Ok(event) = event else { continue };
                if matches!(event.kind, EventKind::Access(_)) {
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(750));
                while event_rx.try_recv().is_ok() {}
                let _ = run_index_job(
                    &db_path,
                    &folder,
                    &scan_lock,
                    &scan_cancel,
                    &app_handle,
                    &network,
                );
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(FolderWatcher { _watcher: watcher })
}

#[tauri::command]
fn get_snapshot(state: State<'_, AppState>) -> Result<AppSnapshot, String> {
    snapshot(&open_db(&state)?)
}

#[tauri::command]
fn search_catalog(query: String, state: State<'_, AppState>) -> Result<Vec<SharedFile>, String> {
    load_files(&open_db(&state)?, Some(&query))
}

#[tauri::command]
async fn set_napstr_folder(
    path: String,
    state: State<'_, AppState>,
) -> Result<IndexReport, String> {
    let folder = PathBuf::from(&path);
    if !folder.is_dir() {
        return Err("The selected Napstr folder does not exist or is not a directory".into());
    }
    let connection = open_db(&state)?;
    connection
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('shared_folder', ?1)",
            [&path],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute("DELETE FROM files", [])
        .map_err(|error| error.to_string())?;
    drop(connection);
    let _ = state.app_handle.emit(LIBRARY_CHANGED_EVENT, ());
    state.network.queue_catalogue_publish(false);
    let db_path = state
        .db_path
        .lock()
        .map_err(|_| "database lock poisoned")?
        .clone();
    *state
        .watcher
        .lock()
        .map_err(|_| "folder watcher lock poisoned")? = Some(start_folder_watcher(
        folder,
        db_path.clone(),
        state.network.clone(),
        state.scan_lock.clone(),
        state.scan_cancel.clone(),
        state.app_handle.clone(),
    )?);
    let scan_lock = state.scan_lock.clone();
    let scan_cancel = state.scan_cancel.clone();
    let app_handle = state.app_handle.clone();
    let network = state.network.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_index_job(
            &db_path,
            Path::new(&path),
            &scan_lock,
            &scan_cancel,
            &app_handle,
            &network,
        )
    })
    .await
    .map_err(|error| format!("library scan task failed: {error}"))?
}

#[tauri::command]
async fn rescan_napstr_folder(state: State<'_, AppState>) -> Result<IndexReport, String> {
    let connection = open_db(&state)?;
    let folder = PathBuf::from(get_setting(&connection, "shared_folder")?);
    drop(connection);
    let db_path = state
        .db_path
        .lock()
        .map_err(|_| "database lock poisoned")?
        .clone();
    let scan_lock = state.scan_lock.clone();
    let scan_cancel = state.scan_cancel.clone();
    let app_handle = state.app_handle.clone();
    let network = state.network.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_index_job(
            &db_path,
            &folder,
            &scan_lock,
            &scan_cancel,
            &app_handle,
            &network,
        )
    })
    .await
    .map_err(|error| format!("library scan task failed: {error}"))?
}

#[tauri::command]
fn cancel_library_scan(state: State<'_, AppState>) {
    state.scan_cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn save_settings(settings: Settings, state: State<'_, AppState>) -> Result<AppSnapshot, String> {
    network::validate_profile_picture(&settings.profile_picture)?;
    validate_length("display name", &settings.display_name, 64)?;
    validate_length("profile about", &settings.profile_about, 500)?;
    validate_length("relay list", &settings.nostr_relays, 4096)?;
    let connection = open_db(&state)?;
    let profile_changed = get_setting(&connection, "display_name")? != settings.display_name
        || get_setting(&connection, "profile_about")? != settings.profile_about
        || get_setting(&connection, "profile_picture")? != settings.profile_picture;
    for (key, value) in [
        ("shared_folder", settings.napstr_folder),
        ("nostr_relays", settings.nostr_relays),
        ("display_name", settings.display_name),
        ("profile_about", settings.profile_about),
        ("profile_picture", settings.profile_picture),
    ] {
        connection
            .execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(|error| error.to_string())?;
    }
    if profile_changed {
        connection
            .execute(
                "UPDATE settings SET value='' WHERE key='profile_event_fingerprint'",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    snapshot(&connection)
}

fn unique_destination(folder: &Path, filename: &str) -> PathBuf {
    let filename = Path::new(filename)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && *value != "." && *value != "..")
        .unwrap_or("download.bin");
    let candidate = folder.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    for suffix in 1..10000 {
        let next = folder.join(format!("{stem} ({suffix}){extension}"));
        if !next.exists() {
            return next;
        }
    }
    folder.join(format!("{stem}-{}{}", Utc::now().timestamp(), extension))
}

#[tauri::command]
fn remove_transfer(id: i64, state: State<'_, AppState>) -> Result<(), String> {
    let connection = open_db(&state)?;
    if id < 0 {
        connection.execute("DELETE FROM download_sources WHERE request_id=(SELECT request_id FROM network_downloads WHERE rowid=?1)", [-id]).map_err(|error| error.to_string())?;
        connection
            .execute("DELETE FROM network_downloads WHERE rowid = ?1", [-id])
            .map_err(|error| error.to_string())?;
    } else {
        connection
            .execute("DELETE FROM transfers WHERE id = ?1", [id])
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn set_downloads_paused(paused: bool, state: State<'_, AppState>) -> Result<(), String> {
    state.network.transfers().set_paused(paused).await;
    Ok(())
}

#[tauri::command]
async fn cancel_transfer(id: i64, state: State<'_, AppState>) -> Result<(), String> {
    if id < 0 {
        state.network.transfers().cancel_by_rowid(-id).await?;
    }
    Ok(())
}

#[tauri::command]
fn get_transfers(state: State<'_, AppState>) -> Result<Vec<Transfer>, String> {
    load_transfers(&open_db(&state)?)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn clean_appimage_environment(command: &mut std::process::Command) {
    let app_dir = std::env::var_os("APPDIR").map(PathBuf::from);
    let host_xdg_data_dirs = std::env::var_os("XDG_DATA_DIRS").and_then(|value| {
        std::env::join_paths(std::env::split_paths(&value).filter(|path| {
            app_dir
                .as_ref()
                .map(|app_dir| !path.starts_with(app_dir))
                .unwrap_or(true)
        }))
        .ok()
    });
    let host_library_path = std::env::var_os("LD_LIBRARY_PATH").and_then(|value| {
        std::env::join_paths(std::env::split_paths(&value).filter(|path| {
            app_dir
                .as_ref()
                .map(|app_dir| !path.starts_with(app_dir))
                .unwrap_or(true)
        }))
        .ok()
    });
    for key in [
        "APPDIR",
        "APPIMAGE",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
        "GTK_DATA_PREFIX",
        "GTK_EXE_PREFIX",
        "GTK_PATH",
        "GTK_IM_MODULE_FILE",
        "GDK_PIXBUF_MODULE_FILE",
        "GIO_EXTRA_MODULES",
        "GSETTINGS_SCHEMA_DIR",
        "GDK_BACKEND",
        "GTK_THEME",
        "XDG_DATA_DIRS",
    ] {
        command.env_remove(key);
    }
    if let Some(value) = host_xdg_data_dirs {
        command.env("XDG_DATA_DIRS", value);
    }
    if let Some(value) = host_library_path {
        command.env("LD_LIBRARY_PATH", value);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch_desktop_opener(command: &mut std::process::Command) -> Result<(), String> {
    use std::process::Stdio;

    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    for _ in 0..12 {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                let mut stderr = String::new();
                if let Some(mut output) = child.stderr.take() {
                    let _ = output.read_to_string(&mut stderr);
                }
                return Err(if stderr.trim().is_empty() {
                    format!("exited with {status}")
                } else {
                    stderr.trim().to_string()
                });
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
    Ok(())
}

fn open_with_system(path: &Path, description: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|error| format!("could not open {description}: {error}"))?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(path)
            .status()
            .map_err(|error| format!("could not open {description}: {error}"))?;
        return status
            .success()
            .then_some(())
            .ok_or_else(|| format!("the system opener could not open {description}"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut failures = Vec::new();
        for (program, prefix) in [("xdg-open", None), ("gio", Some("open"))] {
            let mut command = std::process::Command::new(program);
            clean_appimage_environment(&mut command);
            if let Some(prefix) = prefix {
                command.arg(prefix);
            }
            command.arg(path);
            match launch_desktop_opener(&mut command) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(format!("{program}: {error}")),
            }
        }
        Err(format!(
            "could not open {description} ({})",
            failures.join("; ")
        ))
    }
}

fn is_trusted_release_url(url: &str) -> bool {
    let Some(tag) = url.strip_prefix("https://github.com/lnbits/napstr/releases/tag/") else {
        return false;
    };
    !tag.is_empty()
        && tag.len() <= 100
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

#[tauri::command]
fn open_release_url(url: String) -> Result<(), String> {
    if !is_trusted_release_url(&url) {
        return Err("refused to open an untrusted release URL".into());
    }
    open_with_system(Path::new(&url), "Napstr release")
}

#[tauri::command]
fn open_napstr_folder(state: State<'_, AppState>) -> Result<(), String> {
    let folder = PathBuf::from(get_setting(&open_db(&state)?, "shared_folder")?);
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    open_with_system(&folder, "Napstr folder")
}

fn playable_audio_path(connection: &Connection, file_id: &str) -> Result<PathBuf, String> {
    let blocked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM blocked_files WHERE file_id=?1)",
            [file_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if blocked {
        return Err("playback rejected because this file hash is blocked".into());
    }
    let mut statement = connection
        .prepare("SELECT path FROM files WHERE file_id=?1")
        .map_err(|error| error.to_string())?;
    let paths = statement
        .query_map([file_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    for path in paths {
        let path = PathBuf::from(path.map_err(|error| error.to_string())?);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("audio is not present in the indexed Napstr folder".into())
}

fn validate_length(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.chars().count() > maximum {
        Err(format!("{label} is longer than {maximum} characters"))
    } else {
        Ok(())
    }
}

pub(crate) fn normalise_tags(value: &str) -> Result<String, String> {
    validate_length("tags", value, 256)?;
    let mut seen = HashSet::new();
    let mut tags = Vec::new();
    for value in value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        validate_length("each tag", value, 32)?;
        if value.chars().any(char::is_control) {
            return Err("tags cannot contain control characters".into());
        }
        let key = value.to_lowercase();
        if seen.insert(key) {
            tags.push(value);
        }
    }
    if tags.len() > 12 {
        return Err("a track can have at most 12 tags".into());
    }
    Ok(tags.join(", "))
}

#[tauri::command]
fn save_file_tags(
    file_id: String,
    tags: String,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    if !hex::decode(&file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
    {
        return Err("invalid SHA-256 file ID".into());
    }
    let tags = normalise_tags(&tags)?;
    let connection = open_db(&state)?;
    let changed = connection
        .execute(
            "UPDATE files SET tags=?1 WHERE file_id=?2",
            params![tags, file_id],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("track is no longer in the Napstr folder".into());
    }
    snapshot(&connection)
}

#[tauri::command]
async fn start_network(state: State<'_, AppState>) -> Result<network::NetworkStatus, String> {
    let tor = state.tor.clone();
    tauri::async_runtime::spawn(async move {
        let _ = tor.start().await;
    });
    let mut status = state.network.start().await?;
    apply_tor_status(&mut status, state.tor.status().await);
    Ok(status)
}

#[tauri::command]
async fn network_status(state: State<'_, AppState>) -> Result<network::NetworkStatus, String> {
    let mut status = state.network.status().await?;
    let tor_status = state.tor.status().await;
    if !tor_status.running && !tor_status.starting {
        let tor = state.tor.clone();
        tauri::async_runtime::spawn(async move {
            let _ = tor.start().await;
        });
    }
    apply_tor_status(&mut status, tor_status);
    Ok(status)
}

#[tauri::command]
fn recover_after_sleep(state: State<'_, AppState>) -> Result<(), String> {
    if state.recovering_after_sleep.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let player = state.player.clone();
    let network = state.network.clone();
    let tor = state.tor.clone();
    let recovering = state.recovering_after_sleep.clone();
    tauri::async_runtime::spawn(async move {
        let player_reset = tauri::async_runtime::spawn_blocking(move || {
            player.reset_after_sleep();
        });
        let network_recovery = async {
            let mut result = Err("Nostr reconnection did not start".to_string());
            for attempt in 0..3 {
                result = network.restart().await;
                if result.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2 + attempt * 3)).await;
            }
            result
        };
        let (network_result, tor_result, _) =
            tokio::join!(network_recovery, tor.restart(), player_reset);
        let _ = network_result;
        let _ = tor_result;
        recovering.store(false, Ordering::SeqCst);
    });
    Ok(())
}

fn apply_tor_status(status: &mut network::NetworkStatus, tor: tor::TorStatus) {
    status.tor_running = tor.running;
    status.tor_starting = tor.starting;
    status.tor_progress = tor.bootstrap_progress;
    status.tor_error = tor.error;
}

#[tauri::command]
async fn publish_catalogue(state: State<'_, AppState>) -> Result<usize, String> {
    state.network.queue_catalogue_publish(false);
    Ok(0)
}

#[tauri::command]
async fn publish_profile(state: State<'_, AppState>) -> Result<(), String> {
    state.network.publish_profile().await
}

#[tauri::command]
async fn network_search(
    query: String,
    state: State<'_, AppState>,
) -> Result<Vec<network::CatalogueResult>, String> {
    state.network.search(&query).await
}

#[tauri::command]
async fn get_trollbox_messages(
    state: State<'_, AppState>,
) -> Result<Vec<network::TrollboxMessage>, String> {
    state.network.trollbox_messages().await
}

#[tauri::command]
async fn send_trollbox_message(
    content: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.network.send_trollbox_message(content).await
}

#[tauri::command]
async fn get_track_discussion_messages(
    file_id: String,
    subscribe: bool,
    state: State<'_, AppState>,
) -> Result<Vec<network::TrollboxMessage>, String> {
    state
        .network
        .track_discussion_messages(file_id, subscribe)
        .await
}

#[tauri::command]
async fn send_track_discussion_message(
    file_id: String,
    content: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state
        .network
        .send_track_discussion_message(file_id, content)
        .await
}

#[tauri::command]
async fn request_network_download(
    file_id: String,
    source_pubkeys: Vec<String>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state
        .network
        .request_download(file_id, source_pubkeys)
        .await
}

#[tauri::command]
async fn block_file(file_id: String, state: State<'_, AppState>) -> Result<(), String> {
    if hex::decode(&file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
        == false
    {
        return Err("invalid SHA-256 file ID".into());
    }
    let connection = open_db(&state)?;
    connection.execute(
        "INSERT OR REPLACE INTO blocked_files(file_id,reason,created_at) VALUES(?1,'Blocked by user',?2)",
        params![file_id, Utc::now().to_rfc3339()],
    ).map_err(|error| error.to_string())?;
    connection
        .execute("DELETE FROM remote_catalogue WHERE file_id=?1", [&file_id])
        .map_err(|error| error.to_string())?;
    drop(connection);
    state.network.queue_catalogue_publish(false);
    Ok(())
}

#[tauri::command]
fn block_user(pubkey: String, state: State<'_, AppState>) -> Result<(), String> {
    nostr_sdk::PublicKey::from_hex(&pubkey).map_err(|_| "invalid Nostr public key")?;
    let connection = open_db(&state)?;
    connection.execute(
        "INSERT OR REPLACE INTO blocked_pubkeys(pubkey,reason,created_at) VALUES(?1,'Blocked by user',?2)",
        params![pubkey, Utc::now().to_rfc3339()],
    ).map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM remote_catalogue WHERE source_pubkey=?1",
            [&pubkey],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute("DELETE FROM trollbox_events WHERE pubkey=?1", [&pubkey])
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn report_catalogue(
    file_id: String,
    source_pubkey: String,
    event_id: String,
    report_type: String,
    reason: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state
        .network
        .report_catalogue(file_id, source_pubkey, event_id, report_type, reason)
        .await
}

#[tauri::command]
fn minimise_window(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|error| error.to_string())
}
#[tauri::command]
fn toggle_maximise(window: tauri::Window) -> Result<(), String> {
    let maximised = window.is_maximized().map_err(|error| error.to_string())?;
    if maximised {
        window.unmaximize()
    } else {
        window.maximize()
    }
    .map_err(|error| error.to_string())
}
#[tauri::command]
async fn close_window(window: tauri::Window, state: State<'_, AppState>) -> Result<(), String> {
    state.network.stop().await;
    state.tor.stop().await;
    window.close().map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shutdown_services = Arc::new(Mutex::new(None::<ShutdownServices>));
    let setup_shutdown_services = shutdown_services.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            let app_data = app
                .path()
                .app_data_dir()
                .map_err(|error| error.to_string())?;
            let resource_dir = app
                .path()
                .resource_dir()
                .map_err(|error| error.to_string())?;
            let db_path = app_data.join("napstr.sqlite3");
            initialise_database(&db_path, &app_data)?;
            let tor = Arc::new(tor::TorManager::new(app_data, resource_dir));
            let transfers = Arc::new(transfer::TransferService::new(db_path.clone(), tor.clone()));
            let network =
                network::NetworkService::new(db_path.clone(), transfers, app.handle().clone());
            let scan_lock = Arc::new(Mutex::new(()));
            let scan_cancel = Arc::new(AtomicBool::new(false));
            *setup_shutdown_services
                .lock()
                .map_err(|_| "shutdown service lock was poisoned")? = Some(ShutdownServices {
                network: network.clone(),
                tor: tor.clone(),
            });
            let existing_folder = open_connection(&db_path)
                .ok()
                .and_then(|connection| get_setting(&connection, "shared_folder").ok())
                .map(PathBuf::from);
            let startup_folder = existing_folder.clone();
            let watcher = existing_folder.and_then(|folder| {
                if !folder.is_dir() {
                    if let Ok(connection) = open_connection(&db_path) {
                        let _ = connection.execute("DELETE FROM files", []);
                    }
                    return None;
                }
                start_folder_watcher(
                    folder,
                    db_path.clone(),
                    network.clone(),
                    scan_lock.clone(),
                    scan_cancel.clone(),
                    app.handle().clone(),
                )
                .ok()
            });
            app.manage(AppState {
                db_path: Mutex::new(db_path),
                network: network.clone(),
                tor,
                watcher: Mutex::new(watcher),
                scan_lock: scan_lock.clone(),
                scan_cancel: scan_cancel.clone(),
                app_handle: app.handle().clone(),
                player: Arc::new(player::NativePlayer::default()),
                recovering_after_sleep: Arc::new(AtomicBool::new(false)),
            });
            if let Some(folder) = startup_folder.filter(|folder| folder.is_dir()) {
                let startup_db_path = app
                    .path()
                    .app_data_dir()
                    .map_err(|error| error.to_string())?
                    .join("napstr.sqlite3");
                let startup_handle = app.handle().clone();
                tauri::async_runtime::spawn_blocking(move || {
                    let _ = run_index_job(
                        &startup_db_path,
                        &folder,
                        &scan_lock,
                        &scan_cancel,
                        &startup_handle,
                        &network,
                    );
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            search_catalog,
            set_napstr_folder,
            rescan_napstr_folder,
            cancel_library_scan,
            save_file_tags,
            save_settings,
            remove_transfer,
            get_transfers,
            open_napstr_folder,
            open_release_url,
            player::play_audio,
            player::toggle_audio,
            player::stop_audio,
            player::seek_audio,
            player::set_audio_volume,
            player::audio_status,
            set_downloads_paused,
            cancel_transfer,
            start_network,
            network_status,
            recover_after_sleep,
            publish_catalogue,
            publish_profile,
            network_search,
            get_trollbox_messages,
            send_trollbox_message,
            get_track_discussion_messages,
            send_track_discussion_message,
            request_network_download,
            block_file,
            block_user,
            report_catalogue,
            minimise_window,
            toggle_maximise,
            close_window
        ])
        .build(tauri::generate_context!())
        .expect("error while building Napstr");

    let exit_code = app.run_return(|_, _| {});
    if let Some(services) = shutdown_services
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        tauri::async_runtime::block_on(async move {
            services.network.stop().await;
            services.tor.stop().await;
        });
    }
    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("napstr-{name}-{}", std::process::id()))
    }

    #[test]
    fn release_links_are_limited_to_this_repository() {
        assert!(is_trusted_release_url(
            "https://github.com/lnbits/napstr/releases/tag/v0.2.3"
        ));
        assert!(is_trusted_release_url(
            "https://github.com/lnbits/napstr/releases/tag/v0.3.0-beta.1"
        ));
        assert!(!is_trusted_release_url(
            "https://github.com/another/repository/releases/tag/v0.2.3"
        ));
        assert!(!is_trusted_release_url(
            "https://github.com/lnbits/napstr/releases/tag/v0.2.3?redirect=bad"
        ));
    }

    #[test]
    fn hashes_file_deterministically() {
        let directory = test_directory("hash-test");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("hello.txt");
        fs::write(&path, b"abc").unwrap();
        let (file_hash, size) = hash_file(&path).unwrap();
        assert_eq!(
            file_hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recursive_index_keeps_only_valid_audio() {
        let directory = test_directory("audio-index-test");
        let child = directory.join("artist/album");
        fs::create_dir_all(&child).unwrap();
        let audio = child.join("track.wav");
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\x04\0\0\0song");
        fs::write(&audio, bytes).unwrap();
        fs::write(directory.join("renamed-video.mp3"), b"not audio").unwrap();
        fs::write(child.join("cover.jpg"), b"image").unwrap();
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let mut connection = open_connection(&db_path).unwrap();
        let report = index_path(&mut connection, &directory).unwrap();
        assert_eq!(report.file_count, 1);
        assert_eq!(report.error_count, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.changed_files, 1);
        let indexed = load_files(&connection, None).unwrap();
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].folder, "artist/album");
        connection
            .execute(
                "UPDATE files SET tags='punk, favourite' WHERE file_id=?1",
                [&indexed[0].file_id],
            )
            .unwrap();
        let report = index_path(&mut connection, &directory).unwrap();
        assert_eq!(report.file_count, 1);
        assert_eq!(report.changed_files, 0);
        let tagged = load_files(&connection, Some("FAVOURITE")).unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].tags, "punk, favourite");
        fs::remove_file(&audio).unwrap();
        let report = index_path(&mut connection, &directory).unwrap();
        assert_eq!(report.file_count, 0);
        assert_eq!(report.changed_files, 1);
        assert!(load_files(&connection, None).unwrap().is_empty());
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn indexing_skips_unrelated_files_and_bounds_error_details() {
        let directory = test_directory("bounded-index-errors-test");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("cover.jpg"), b"not audio").unwrap();
        for index in 0..(MAX_INDEX_ERRORS + 5) {
            fs::write(directory.join(format!("invalid-{index}.mp3")), b"not audio").unwrap();
        }
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let mut connection = open_connection(&db_path).unwrap();

        let report = index_path(&mut connection, &directory).unwrap();

        assert_eq!(report.file_count, 0);
        assert_eq!(report.error_count, MAX_INDEX_ERRORS + 5);
        assert_eq!(report.errors.len(), MAX_INDEX_ERRORS);
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn indexing_can_be_cancelled_before_database_changes() {
        let directory = test_directory("cancel-index-test");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("track.mp3"), b"not audio").unwrap();
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let mut connection = open_connection(&db_path).unwrap();
        let cancelled = AtomicBool::new(true);

        let result =
            index_path_with_progress(&mut connection, &directory, &cancelled, |_, _| {}, |_| {});

        assert_eq!(result.unwrap_err(), "Indexing cancelled");
        assert!(load_files(&connection, None).unwrap().is_empty());
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn indexing_commits_verified_files_in_bounded_batches() {
        let directory = test_directory("progressive-index-test");
        fs::create_dir_all(&directory).unwrap();
        for index in 0..(INDEX_COMMIT_BATCH_SIZE + 1) {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&40u32.to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\x04\0\0\0");
            bytes.extend_from_slice(&(index as u32).to_le_bytes());
            fs::write(directory.join(format!("track-{index:03}.wav")), bytes).unwrap();
        }
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let mut connection = open_connection(&db_path).unwrap();
        let mut committed_batches = Vec::new();
        let mut independently_visible = Vec::new();

        let report = index_path_with_progress(
            &mut connection,
            &directory,
            &AtomicBool::new(false),
            |_, _| {},
            |batch| {
                committed_batches.push(batch.files.len());
                let observer = open_connection(&db_path).unwrap();
                independently_visible.push(load_files(&observer, None).unwrap().len());
            },
        )
        .unwrap();

        assert_eq!(report.file_count, INDEX_COMMIT_BATCH_SIZE + 1);
        assert_eq!(committed_batches, vec![INDEX_COMMIT_BATCH_SIZE, 1]);
        assert_eq!(
            independently_visible,
            vec![INDEX_COMMIT_BATCH_SIZE, INDEX_COMMIT_BATCH_SIZE + 1]
        );
        assert_eq!(
            load_files(&connection, None).unwrap().len(),
            INDEX_COMMIT_BATCH_SIZE + 1
        );
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancelling_after_a_batch_keeps_only_verified_commits() {
        let directory = test_directory("progressive-cancel-test");
        fs::create_dir_all(&directory).unwrap();
        for index in 0..(INDEX_COMMIT_BATCH_SIZE + 1) {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&40u32.to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\x04\0\0\0");
            bytes.extend_from_slice(&(index as u32).to_le_bytes());
            fs::write(directory.join(format!("track-{index:03}.wav")), bytes).unwrap();
        }
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let mut connection = open_connection(&db_path).unwrap();
        let cancelled = AtomicBool::new(false);

        let result = index_path_with_progress(
            &mut connection,
            &directory,
            &cancelled,
            |_, _| {},
            |_| cancelled.store(true, Ordering::SeqCst),
        );

        assert_eq!(result.unwrap_err(), "Indexing cancelled");
        assert_eq!(
            load_files(&connection, None).unwrap().len(),
            INDEX_COMMIT_BATCH_SIZE
        );
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn search_is_word_based_case_insensitive_and_typo_tolerant() {
        let fields = ["Metallica_Enter_Sandman_Official_Music_Video.mp3"];
        assert!(search_matches("METALLICA", &fields));
        assert!(search_matches("enter sandman", &fields));
        assert!(search_matches("sandman metallica", &fields));
        assert!(search_matches("metalica", &fields));
        assert!(!search_matches("metallica mario", &fields));
    }

    #[test]
    fn normalises_and_limits_track_tags() {
        assert_eq!(
            normalise_tags(" punk, Live ,PUNK, audiobook ").unwrap(),
            "punk, Live, audiobook"
        );
        assert!(normalise_tags("bad\ntag").is_err());
        let too_many = (0..13)
            .map(|index| format!("tag{index}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(normalise_tags(&too_many).is_err());
        assert!(normalise_tags(&"x".repeat(33)).is_err());
    }

    #[test]
    fn fresh_install_uses_one_napstr_folder() {
        let directory = test_directory("default-folder-test");
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        let settings = load_settings(&connection).unwrap();
        assert_eq!(
            Path::new(&settings.napstr_folder),
            directory.join("Downloads")
        );
        assert!(Path::new(&settings.napstr_folder).is_dir());
        assert!(get_setting(&connection, "download_folder").is_err());
        assert_eq!(settings.nostr_relays, DEFAULT_NOSTR_RELAYS);
        assert_eq!(
            settings.profile_about,
            "Sharing files privately with Napstr. napstr.net"
        );
        assert_eq!(
            get_setting(&connection, "profile_event_fingerprint").unwrap(),
            ""
        );
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_default_profile_is_migrated_and_queued_for_publication() {
        let directory = test_directory("default-profile-migration-test");
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        connection
            .execute(
                "UPDATE settings SET value='Sharing files privately with Napstr.' WHERE key='profile_about'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE settings SET value='already-published' WHERE key='profile_event_fingerprint'",
                [],
            )
            .unwrap();
        drop(connection);

        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        assert_eq!(
            get_setting(&connection, "profile_about").unwrap(),
            "Sharing files privately with Napstr. napstr.net"
        );
        assert_eq!(
            get_setting(&connection, "profile_event_fingerprint").unwrap(),
            ""
        );
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn playback_uses_only_files_indexed_from_the_napstr_folder() {
        let directory = test_directory("completed-playback-test");
        fs::create_dir_all(&directory).unwrap();
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let audio = directory.join("new-download.mp3");
        fs::write(&audio, b"downloaded audio placeholder").unwrap();
        let file_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let connection = open_connection(&db_path).unwrap();
        connection
            .execute(
                "INSERT INTO network_downloads
                 (request_id,file_id,source_pubkey,filename,size,progress,status,speed,destination,onion,updated_at)
                 VALUES ('request',?1,'source','new-download.mp3',28,100,'Verified · Complete','—',?2,'example.onion','now')",
                params![file_id, audio.to_string_lossy()],
            )
            .unwrap();
        assert!(playable_audio_path(&connection, file_id).is_err());
        connection
            .execute(
                "INSERT INTO files
                 (file_id,filename,path,size,format,indexed_at,mime,folder)
                 VALUES (?1,'new-download.mp3',?2,28,'MP3','now','audio/mpeg','')",
                params![file_id, audio.to_string_lossy()],
            )
            .unwrap();
        assert_eq!(playable_audio_path(&connection, file_id).unwrap(), audio);
        fs::remove_file(&audio).unwrap();
        assert!(playable_audio_path(&connection, file_id).is_err());
        drop(connection);

        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        let completed: i64 = connection
            .query_row(
                "SELECT count(*) FROM network_downloads WHERE status='Verified · Complete'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(completed, 0);
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_default_relays_expand_without_overwriting_custom_relays() {
        let directory = test_directory("default-relay-migration-test");
        let db_path = directory.join("napstr.sqlite3");
        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        connection
            .execute(
                "UPDATE settings SET value=?1 WHERE key='nostr_relays'",
                [LEGACY_DEFAULT_NOSTR_RELAYS],
            )
            .unwrap();
        drop(connection);
        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        assert_eq!(
            get_setting(&connection, "nostr_relays").unwrap(),
            DEFAULT_NOSTR_RELAYS
        );
        connection
            .execute(
                "UPDATE settings SET value='wss://my-relay.example' WHERE key='nostr_relays'",
                [],
            )
            .unwrap();
        drop(connection);
        initialise_database(&db_path, &directory).unwrap();
        let connection = open_connection(&db_path).unwrap();
        assert_eq!(
            get_setting(&connection, "nostr_relays").unwrap(),
            "wss://my-relay.example"
        );
        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }
}
