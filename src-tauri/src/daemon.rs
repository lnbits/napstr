use napstr_lib::{
    events::BroadcasterEmitter,
    get_setting, index_path_headless, initialise_database,
    network::NetworkService,
    open_connection,
    server::{create_router, ServerState},
    start_folder_watcher_headless,
    tor::TorManager,
    transfer::TransferService,
};
use std::{
    env,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎵 Starting Napstr Sovereign Seeder & Web Server...");

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(30421);

    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());

    let data_dir = env::var("DATA_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_dir()
                .map(|p| p.join("napstr"))
                .unwrap_or_else(|| PathBuf::from("./data"))
        });

    fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("napstr.sqlite3");
    initialise_database(&db_path, &data_dir)?;

    let resource_dir = env::var("RESOURCE_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.clone());

    let (broadcast_tx, _) = broadcast::channel(256);
    let event_emitter = Arc::new(BroadcasterEmitter::new(broadcast_tx.clone()));

    let tor = Arc::new(TorManager::new(data_dir.clone(), resource_dir));
    let transfers = Arc::new(TransferService::new(db_path.clone(), tor.clone()));
    let network = NetworkService::new(db_path.clone(), transfers, event_emitter);

    // Check shared folder from environment or database
    let music_dir_env = env::var("MUSIC_DIR").ok().map(PathBuf::from);
    let existing_folder = open_connection(&db_path)
        .ok()
        .and_then(|conn| get_setting(&conn, "shared_folder").ok())
        .map(PathBuf::from)
        .or(music_dir_env);

    let watcher = if let Some(folder) = existing_folder {
        if folder.is_dir() {
            println!("📂 Indexing music library at: {}", folder.display());
            if let Ok(mut conn) = open_connection(&db_path) {
                let _ = index_path_headless(&mut conn, &folder);
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('shared_folder', ?1)",
                    [folder.to_string_lossy().as_ref()],
                );
            }
            start_folder_watcher_headless(folder, db_path.clone(), network.clone()).ok()
        } else {
            None
        }
    } else {
        None
    };

    let static_dir = env::var("STATIC_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let candidates = ["./build", "./dist", "../build", "../dist", "./website"];
            candidates.iter().map(PathBuf::from).find(|p| p.exists())
        });

    if let Some(ref dir) = static_dir {
        println!("🌐 Serving web UI from: {}", dir.display());
    }

    // Auto-start Tor and Nostr network in background
    let tor_clone = tor.clone();
    tokio::spawn(async move {
        println!("🧅 Starting Tor onion controller...");
        if let Err(e) = tor_clone.start().await {
            eprintln!("⚠️ Tor start error: {e}");
        }
    });

    let network_clone = network.clone();
    tokio::spawn(async move {
        println!("📡 Connecting to Nostr relays...");
        if let Err(e) = network_clone.start().await {
            eprintln!("⚠️ Nostr network start error: {e}");
        }
    });

    let server_state = Arc::new(ServerState {
        db_path: Mutex::new(db_path),
        network: network.clone(),
        tor: tor.clone(),
        watcher: Mutex::new(watcher),
        broadcast_tx,
    });

    let app = create_router(server_state, static_dir);
    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    println!("🚀 Napstr daemon listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::select! {
        res = axum::serve(listener, app) => {
            if let Err(e) = res {
                eprintln!("Server error: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\n🛑 Graceful shutdown initiated...");
            network.stop().await;
            tor.stop().await;
            println!("👋 Napstr stopped cleanly.");
        }
    }

    Ok(())
}
