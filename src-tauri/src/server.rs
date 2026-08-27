use crate::{
    get_setting, index_path_headless, load_files, load_transfers,
    network::{self, NetworkService},
    normalise_tags, open_connection, playable_audio_path, snapshot,
    tor::TorManager,
    validate_length, AppSnapshot, FolderWatcher, IndexReport, Settings, SharedFile, Transfer,
};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

pub struct ServerState {
    pub db_path: Mutex<PathBuf>,
    pub network: Arc<NetworkService>,
    pub tor: Arc<TorManager>,
    pub watcher: Mutex<Option<FolderWatcher>>,
    pub broadcast_tx: broadcast::Sender<String>,
}

impl ServerState {
    pub fn open_db(&self) -> Result<rusqlite::Connection, String> {
        let path = self
            .db_path
            .lock()
            .map_err(|_| "database lock poisoned")?
            .clone();
        open_connection(&path)
    }
}

pub type SharedServerState = Arc<ServerState>;

#[derive(Deserialize)]
pub struct SearchQuery {
    pub query: Option<String>,
}

#[derive(Deserialize)]
pub struct SetFolderPayload {
    pub path: String,
}

#[derive(Deserialize)]
pub struct SaveTagsPayload {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub tags: String,
}

#[derive(Deserialize)]
pub struct TransferIdPayload {
    pub id: i64,
}

#[derive(Deserialize)]
pub struct PauseDownloadsPayload {
    pub paused: bool,
}

#[derive(Deserialize)]
pub struct NetworkSearchPayload {
    pub query: String,
}

#[derive(Deserialize)]
pub struct DownloadRequestPayload {
    #[serde(rename = "fileId")]
    pub file_id: String,
    #[serde(rename = "sourcePubkeys")]
    pub source_pubkeys: Vec<String>,
    #[serde(rename = "destinationFolder")]
    pub destination_folder: Option<String>,
}

#[derive(Deserialize)]
pub struct BlockFilePayload {
    #[serde(rename = "fileId")]
    pub file_id: String,
}

#[derive(Deserialize)]
pub struct BlockUserPayload {
    pub pubkey: String,
}

#[derive(Deserialize)]
pub struct TrollboxMessagePayload {
    pub content: String,
}

#[derive(Deserialize)]
pub struct TrackDiscussionQuery {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub subscribe: Option<bool>,
}

#[derive(Deserialize)]
pub struct TrackDiscussionMessagePayload {
    #[serde(rename = "fileId")]
    pub file_id: String,
    pub content: String,
}

pub fn create_router(state: SharedServerState, static_dir: Option<PathBuf>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let api_router = Router::new()
        .route("/version", get(handle_version))
        .route("/ws", get(handle_ws))
        .route("/get_snapshot", post(handle_get_snapshot))
        .route("/search_catalog", post(handle_search_catalog))
        .route("/set_napstr_folder", post(handle_set_napstr_folder))
        .route("/rescan_napstr_folder", post(handle_rescan_napstr_folder))
        .route("/save_settings", post(handle_save_settings))
        .route("/save_file_tags", post(handle_save_file_tags))
        .route("/start_network", post(handle_start_network))
        .route("/network_status", post(handle_network_status))
        .route("/network_search", post(handle_network_search))
        .route("/publish_catalogue", post(handle_publish_catalogue))
        .route("/publish_profile", post(handle_publish_profile))
        .route("/get_transfers", post(handle_get_transfers))
        .route("/request_network_download", post(handle_request_network_download))
        .route("/cancel_transfer", post(handle_cancel_transfer))
        .route("/remove_transfer", post(handle_remove_transfer))
        .route("/set_downloads_paused", post(handle_set_downloads_paused))
        .route("/block_file", post(handle_block_file))
        .route("/block_user", post(handle_block_user))
        .route("/get_trollbox_messages", post(handle_get_trollbox_messages))
        .route("/send_trollbox_message", post(handle_send_trollbox_message))
        .route("/get_track_discussion_messages", post(handle_get_track_discussion_messages))
        .route("/send_track_discussion_message", post(handle_send_track_discussion_message))
        .route("/recover_after_sleep", post(handle_recover_after_sleep))
        .route("/stream/{file_id}", get(handle_audio_stream));

    let mut app = Router::new().nest("/api", api_router).layer(cors);

    if let Some(dir) = static_dir {
        if dir.exists() {
            app = app.fallback_service(ServeDir::new(dir));
        }
    }

    app.with_state(state)
}

async fn handle_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": "0.1.0-umbrel" }))
}

async fn handle_get_snapshot(
    State(state): State<SharedServerState>,
) -> Result<Json<AppSnapshot>, (StatusCode, String)> {
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let snap = snapshot(&conn).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(snap))
}

async fn handle_search_catalog(
    State(state): State<SharedServerState>,
    Json(payload): Json<SearchQuery>,
) -> Result<Json<Vec<SharedFile>>, (StatusCode, String)> {
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let files = load_files(&conn, payload.query.as_deref())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(files))
}

async fn handle_set_napstr_folder(
    State(state): State<SharedServerState>,
    Json(payload): Json<SetFolderPayload>,
) -> Result<Json<IndexReport>, (StatusCode, String)> {
    let folder = PathBuf::from(&payload.path);
    let mut conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let report = index_path_headless(&mut conn, &folder)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('shared_folder', ?1)",
        [&payload.path],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let db_path = state
        .db_path
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "db lock poisoned".into()))?
        .clone();
    *state
        .watcher
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "watcher lock poisoned".into()))? =
        crate::start_folder_watcher_headless(folder, db_path, state.network.clone()).ok();

    Ok(Json(report))
}

async fn handle_rescan_napstr_folder(
    State(state): State<SharedServerState>,
) -> Result<Json<IndexReport>, (StatusCode, String)> {
    let mut conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let folder_str = get_setting(&conn, "shared_folder")
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let folder = PathBuf::from(folder_str);
    let report = index_path_headless(&mut conn, &folder)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(report))
}

async fn handle_save_settings(
    State(state): State<SharedServerState>,
    Json(settings): Json<Settings>,
) -> Result<Json<AppSnapshot>, (StatusCode, String)> {
    network::validate_profile_picture(&settings.profile_picture)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    validate_length("display name", &settings.display_name, 64)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    validate_length("profile about", &settings.profile_about, 500)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    validate_length("relay list", &settings.nostr_relays, 4096)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let profile_changed = get_setting(&conn, "display_name").unwrap_or_default() != settings.display_name
        || get_setting(&conn, "profile_about").unwrap_or_default() != settings.profile_about
        || get_setting(&conn, "profile_picture").unwrap_or_default() != settings.profile_picture;

    for (key, value) in [
        ("display_name", settings.display_name.as_str()),
        ("profile_about", settings.profile_about.as_str()),
        ("profile_picture", settings.profile_picture.as_str()),
        ("nostr_relays", settings.nostr_relays.as_str()),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            [key, value],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    if profile_changed {
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('profile_event_fingerprint', '')",
            [],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let snap = snapshot(&conn).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(snap))
}

async fn handle_save_file_tags(
    State(state): State<SharedServerState>,
    Json(payload): Json<SaveTagsPayload>,
) -> Result<Json<AppSnapshot>, (StatusCode, String)> {
    if !hex::decode(&payload.file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid SHA-256 file ID".into()));
    }
    let tags = normalise_tags(&payload.tags).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let changed = conn
        .execute(
            "UPDATE files SET tags=?1 WHERE file_id=?2",
            rusqlite::params![tags, payload.file_id],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if changed != 1 {
        return Err((StatusCode::NOT_FOUND, "track is no longer in folder".into()));
    }
    let snap = snapshot(&conn).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(snap))
}

async fn handle_start_network(
    State(state): State<SharedServerState>,
) -> Result<Json<network::NetworkStatus>, (StatusCode, String)> {
    let tor = state.tor.clone();
    tokio::spawn(async move {
        let _ = tor.start().await;
    });
    let mut status = state
        .network
        .start()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let tor_status = state.tor.status().await;
    status.tor_running = tor_status.running;
    status.tor_starting = tor_status.starting;
    status.tor_progress = tor_status.bootstrap_progress;
    status.tor_error = tor_status.error;
    Ok(Json(status))
}

async fn handle_network_status(
    State(state): State<SharedServerState>,
) -> Result<Json<network::NetworkStatus>, (StatusCode, String)> {
    let mut status = state
        .network
        .status()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let tor_status = state.tor.status().await;
    if !tor_status.running && !tor_status.starting {
        let tor = state.tor.clone();
        tokio::spawn(async move {
            let _ = tor.start().await;
        });
    }
    status.tor_running = tor_status.running;
    status.tor_starting = tor_status.starting;
    status.tor_progress = tor_status.bootstrap_progress;
    status.tor_error = tor_status.error;
    Ok(Json(status))
}

async fn handle_network_search(
    State(state): State<SharedServerState>,
    Json(payload): Json<NetworkSearchPayload>,
) -> Result<Json<Vec<network::CatalogueResult>>, (StatusCode, String)> {
    let results = state
        .network
        .search(&payload.query)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(results))
}

async fn handle_publish_catalogue(
    State(state): State<SharedServerState>,
) -> Result<Json<usize>, (StatusCode, String)> {
    // 0.1.4: catalogue publishing is queued/backgrounded (returns no count).
    state.network.queue_catalogue_publish(false);
    Ok(Json(0))
}

async fn handle_publish_profile(
    State(state): State<SharedServerState>,
) -> Result<Json<()>, (StatusCode, String)> {
    state
        .network
        .publish_profile()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(()))
}

async fn handle_get_transfers(
    State(state): State<SharedServerState>,
) -> Result<Json<Vec<Transfer>>, (StatusCode, String)> {
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let transfers = load_transfers(&conn).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(transfers))
}

async fn handle_request_network_download(
    State(state): State<SharedServerState>,
    Json(payload): Json<DownloadRequestPayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    state
        .network
        .request_download(
            payload.file_id,
            payload.source_pubkeys,
            payload.destination_folder,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(()))
}

async fn handle_cancel_transfer(
    State(state): State<SharedServerState>,
    Json(payload): Json<TransferIdPayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    if payload.id < 0 {
        state
            .network
            .transfers()
            .cancel_by_rowid(-payload.id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    Ok(Json(()))
}

async fn handle_remove_transfer(
    State(state): State<SharedServerState>,
    Json(payload): Json<TransferIdPayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if payload.id < 0 {
        conn.execute(
            "DELETE FROM download_sources WHERE request_id=(SELECT request_id FROM network_downloads WHERE rowid=?1)",
            [-payload.id],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        conn.execute(
            "DELETE FROM network_downloads WHERE rowid = ?1",
            [-payload.id],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    } else {
        conn.execute("DELETE FROM transfers WHERE id = ?1", [payload.id])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(()))
}

async fn handle_set_downloads_paused(
    State(state): State<SharedServerState>,
    Json(payload): Json<PauseDownloadsPayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    state.network.transfers().set_paused(payload.paused).await;
    Ok(Json(()))
}

async fn handle_block_file(
    State(state): State<SharedServerState>,
    Json(payload): Json<BlockFilePayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    if !hex::decode(&payload.file_id)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid SHA-256 file ID".into()));
    }
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    conn.execute(
        "INSERT OR REPLACE INTO blocked_files(file_id,reason,created_at) VALUES(?1,'Blocked by user',?2)",
        rusqlite::params![payload.file_id, chrono::Utc::now().to_rfc3339()],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute("DELETE FROM remote_catalogue WHERE file_id=?1", [&payload.file_id])
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    drop(conn);
    if state.network.status().await.map(|s| s.connected).unwrap_or(false) {
        let _ = state.network.queue_catalogue_publish(false);
    }
    Ok(Json(()))
}

async fn handle_block_user(
    State(state): State<SharedServerState>,
    Json(payload): Json<BlockUserPayload>,
) -> Result<Json<()>, (StatusCode, String)> {
    nostr_sdk::PublicKey::from_hex(&payload.pubkey)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid Nostr public key".into()))?;
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    conn.execute(
        "INSERT OR REPLACE INTO blocked_pubkeys(pubkey,reason,created_at) VALUES(?1,'Blocked by user',?2)",
        rusqlite::params![payload.pubkey, chrono::Utc::now().to_rfc3339()],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute(
        "DELETE FROM remote_catalogue WHERE source_pubkey=?1",
        [&payload.pubkey],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    conn.execute("DELETE FROM trollbox_events WHERE pubkey=?1", [&payload.pubkey])
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(()))
}

async fn handle_get_trollbox_messages(
    State(state): State<SharedServerState>,
) -> Result<Json<Vec<network::TrollboxMessage>>, (StatusCode, String)> {
    let msgs = state
        .network
        .trollbox_messages()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(msgs))
}

async fn handle_send_trollbox_message(
    State(state): State<SharedServerState>,
    Json(payload): Json<TrollboxMessagePayload>,
) -> Result<Json<String>, (StatusCode, String)> {
    let id = state
        .network
        .send_trollbox_message(payload.content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(id))
}

async fn handle_get_track_discussion_messages(
    State(state): State<SharedServerState>,
    Json(payload): Json<TrackDiscussionQuery>,
) -> Result<Json<Vec<network::TrollboxMessage>>, (StatusCode, String)> {
    let msgs = state
        .network
        .track_discussion_messages(payload.file_id, payload.subscribe.unwrap_or(true))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(msgs))
}

async fn handle_send_track_discussion_message(
    State(state): State<SharedServerState>,
    Json(payload): Json<TrackDiscussionMessagePayload>,
) -> Result<Json<String>, (StatusCode, String)> {
    let id = state
        .network
        .send_track_discussion_message(payload.file_id, payload.content)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(id))
}

async fn handle_recover_after_sleep(
    State(state): State<SharedServerState>,
) -> Result<Json<()>, (StatusCode, String)> {
    let network = state.network.clone();
    let tor = state.tor.clone();
    tokio::spawn(async move {
        let _ = network.restart().await;
        let _ = tor.restart().await;
    });
    Ok(Json(()))
}

async fn handle_ws(
    ws: WebSocketUpgrade,
    State(state): State<SharedServerState>,
) -> Response {
    ws.on_upgrade(|socket| websocket_loop(socket, state))
}

async fn websocket_loop(socket: WebSocket, state: SharedServerState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.broadcast_tx.subscribe();

    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_msg)) = receiver.next().await {}
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
}

async fn handle_audio_stream(
    Path(file_id): Path<String>,
    State(state): State<SharedServerState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let conn = state.open_db().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let path = playable_audio_path(&conn, &file_id)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;

    let mut file = File::open(&path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("open error: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("metadata error: {e}")))?;
    let file_size = metadata.len();

    let mime_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range) = range_header {
        if let Some(spec) = range.strip_prefix("bytes=") {
            let parts: Vec<&str> = spec.split('-').collect();
            let start: u64 = parts[0].parse().unwrap_or(0);
            let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                parts[1].parse().unwrap_or(file_size - 1)
            } else {
                file_size - 1
            };

            let end = end.min(file_size - 1);
            if start > end || start >= file_size {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{file_size}"))],
                    Body::empty(),
                )
                    .into_response());
            }

            let length = end - start + 1;
            file.seek(SeekFrom::Start(start))
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("seek error: {e}")))?;

            let mut buffer = vec![0u8; length as usize];
            file.read_exact(&mut buffer)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")))?;

            return Ok((
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime_type),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{file_size}"),
                    ),
                    (header::CONTENT_LENGTH, length.to_string()),
                ],
                Body::from(buffer),
            )
                .into_response());
        }
    }

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_type),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_LENGTH, file_size.to_string()),
        ],
        Body::from(buffer),
    )
        .into_response())
}
