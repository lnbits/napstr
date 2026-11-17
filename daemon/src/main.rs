// Compile the existing core directly, without a dependency on the desktop.
#[path = "../../src-tauri/src/audio.rs"]
mod audio;
mod config;
mod control;
mod downloads;
mod library;
mod mobile;
mod network;
#[path = "../../src-tauri/src/protocol.rs"]
mod protocol;
mod server;
#[allow(dead_code)]
#[path = "../../src-tauri/src/tor.rs"]
mod tor;

use config::Config;
use nostr_sdk::prelude::*;
use std::{
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    task::JoinSet,
    time::{sleep, timeout},
};

const HELP: &str = "napstrd - headless Napstr music seeder

Usage: napstrd [--check | --version | --help]
       napstrd pair [--read-only] [--qr-file PATH] [--json]
       napstrd devices [--json]
       napstrd revoke ENDPOINT_ID [--json]

Administration commands contact the running daemon through NAPSTR_DATA_DIR.
They do not need the nsec or music folder environment variables.

Required environment:
  NAPSTR_NSEC           Nostr private key in nsec format
  NAPSTR_MUSIC_DIR      Absolute path to an existing music/download folder

Optional environment:
  NAPSTR_DATA_DIR       Private writable state directory (default /var/lib/napstrd)
  NAPSTR_RELAYS         Comma-separated wss:// relay URLs
  NAPSTR_SCAN_INTERVAL  Rescan interval in seconds (10..86400; default 60)
  NAPSTR_TOR_PATH       Tor executable (default tor on PATH)

--check validates configuration and writable state without connecting.
Run in the foreground or use the supplied systemd service. SIGINT/SIGTERM stop it.
";

fn main() {
    if let Err(error) = entry() {
        eprintln!("napstrd: {error}");
        std::process::exit(1);
    }
}

fn entry() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(command) = control::Command::parse(&args)? {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(command.run());
    }
    match args.as_slice() {
        [arg] if arg == "--help" || arg == "-h" => {
            print!("{HELP}");
            return Ok(());
        }
        [arg] if arg == "--version" || arg == "-V" => {
            println!(
                "napstrd {}",
                option_env!("NAPSTR_DAEMON_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
            );
            return Ok(());
        }
        [] => {}
        [arg] if arg == "--check" => {}
        _ => return Err("unknown arguments; use --help".into()),
    }
    let mut config = Config::from_env()?;
    // Before runtime threads start, remove the key from child environments.
    std::env::remove_var("NAPSTR_NSEC");
    config.data_dir = resolve_data_dir(&config.data_dir, &config.music_dir)?;
    let _lock = prepare_data_dir(&config.data_dir)?;
    if args.first().is_some_and(|arg| arg == "--check") {
        println!(
            "Configuration OK; public identity: {}",
            config
                .keys
                .public_key()
                .to_bech32()
                .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = runtime.block_on(run(config));
    // A scan or snapshot may still be finishing on a blocking worker.
    runtime.shutdown_timeout(Duration::from_secs(5));
    result
}

fn resolve_data_dir(path: &Path, music: &Path) -> Result<PathBuf, String> {
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("NAPSTR_DATA_DIR must not contain '..'".into());
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(existing.file_name().ok_or("invalid data directory")?);
        existing = existing.parent().ok_or("invalid data directory")?;
    }
    let mut canonical = existing.canonicalize().map_err(|e| e.to_string())?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    if canonical.starts_with(music) || music.starts_with(&canonical) {
        return Err(
            "NAPSTR_DATA_DIR and NAPSTR_MUSIC_DIR must be separate, non-nested directories".into(),
        );
    }
    Ok(canonical)
}

fn prepare_data_dir(path: &Path) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("cannot create daemon data directory: {e}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join("daemon.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| "another napstrd is using NAPSTR_DATA_DIR")?;
    Ok(lock)
}

async fn run(config: Config) -> Result<(), String> {
    let tor = Arc::new(tor::TorManager::new(
        config.data_dir.clone(),
        config.data_dir.join("resources"),
    ));
    let client = Client::builder()
        .signer(config.keys.clone())
        .database(MemoryDatabase::with_opts(MemoryDatabaseOptions {
            events: true,
            max_events: Some(4096),
        }))
        .build();
    eprintln!(
        "napstrd starting; public identity: {}",
        config
            .keys
            .public_key()
            .to_bech32()
            .map_err(|e| e.to_string())?
    );
    let library = Arc::new(RwLock::new(library::Library::new()));
    let downloads = downloads::Downloads::new(
        client.clone(),
        tor.clone(),
        config.music_dir.clone(),
        &config.data_dir,
        library.clone(),
    )?;
    let mobile = mobile::MobileService::new(
        library.clone(),
        config.music_dir.clone(),
        config.data_dir.clone(),
        downloads.clone(),
    )?;
    let control = control::ControlSocket::bind(&config.data_dir)?;
    let mut administration = JoinSet::new();
    administration.spawn(control.serve(mobile.clone()));
    if !mobile.devices()?.is_empty() {
        mobile.start().await?;
    }
    let result = tokio::select! {
        result = service(&config, tor.clone(), client.clone(), library, mobile.clone(), downloads) => result,
        result = administration.join_next() => match result {
            Some(Ok(result)) => result,
            Some(Err(error)) => Err(error.to_string()),
            None => Err("Administration service stopped".into()),
        },
        result = shutdown_signal() => result,
    };
    administration.abort_all();
    mobile.stop().await;
    // Dropping the service's JoinSet stops requests and transfers. If relays
    // are offline, published availability expires after ten minutes.
    let _ = timeout(
        Duration::from_secs(3),
        network::availability(&client, &[], 125),
    )
    .await;
    let _ = timeout(Duration::from_secs(3), client.disconnect()).await;
    tor.stop().await;
    eprintln!("napstrd stopped");
    result
}

async fn service(
    config: &Config,
    tor: Arc<tor::TorManager>,
    client: Client,
    library: Arc<RwLock<library::Library>>,
    mobile: Arc<mobile::MobileService>,
    downloads: Arc<downloads::Downloads>,
) -> Result<(), String> {
    let root = config.music_dir.clone();
    let (scanner, files) = tokio::task::spawn_blocking(move || {
        let mut scanner = library::Scanner::default();
        let files = scanner.scan(&root);
        (scanner, files)
    })
    .await
    .map_err(|e| e.to_string())?;
    let files = files?;
    eprintln!("Indexed {} audio files; starting Tor", files.len());
    *library.write().await = files;
    mobile.library_changed();
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| e.to_string())?;
    let onion = tor
        .create_onion(listener.local_addr().map_err(|e| e.to_string())?.port())
        .await?;
    network::connect(&client, config.keys.public_key(), &config.relays).await?;
    let server = Arc::new(server::Server::new(
        library.clone(),
        config.music_dir.clone(),
        config.data_dir.clone(),
    ));
    let mut tasks = JoinSet::new();
    tasks.spawn(server.clone().listen(listener));
    tasks.spawn(network::inbox(
        client.clone(),
        config.keys.public_key(),
        server,
        onion.onion.clone(),
        downloads.clone(),
    ));
    let state = config.data_dir.join(format!(
        "published-{}.json",
        config.keys.public_key().to_hex()
    ));
    tasks.spawn(network::publisher(client.clone(), library.clone(), state));
    tasks.spawn(network::heartbeat(client, library.clone()));
    let rescan = downloads.rescan.clone();
    tasks.spawn(downloads.run());
    let root = config.music_dir.clone();
    let interval = config.scan_interval;
    tasks.spawn(async move {
        let mut scanner = scanner;
        loop {
            tokio::select! { _ = sleep(interval) => {}, _ = rescan.notified() => {} }
            let root = root.clone();
            let revision = mobile.revision();
            let (next, files) = tokio::task::spawn_blocking(move || {
                let files = scanner.scan(&root);
                (scanner, files)
            })
            .await
            .map_err(|e| e.to_string())?;
            scanner = next;
            let files = files?;
            let mut current = library.write().await;
            if mobile.revision() != revision {
                rescan.notify_one();
                continue;
            }
            if *current != files {
                *current = files;
                mobile.library_changed();
            }
        }
    });
    tasks.spawn(async move {
        loop {
            sleep(Duration::from_secs(20)).await;
            if !tor.status().await.running {
                return Err(
                    "Tor exited; restart napstrd to establish a new private session".into(),
                );
            }
        }
    });
    eprintln!("napstrd ready; sharing over Tor and watching the music folder");
    let result = tasks
        .join_next()
        .await
        .ok_or("daemon workers stopped")?
        .map_err(|e| e.to_string())?;
    drop(onion);
    result.and(Err("daemon worker stopped unexpectedly".into()))
}

async fn shutdown_signal() -> Result<(), String> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|e| e.to_string())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|e| e.to_string()),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_separate_private_and_single_instance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let music = root.join("music");
        fs::create_dir(&music).unwrap();
        assert!(resolve_data_dir(&music, &music).is_err());
        assert!(resolve_data_dir(&music.join("state"), &music).is_err());
        assert!(resolve_data_dir(&root, &music).is_err());
        let state = resolve_data_dir(&root.join("state"), &music).unwrap();
        let lock = prepare_data_dir(&state).unwrap();
        assert!(prepare_data_dir(&state).is_err());
        drop(lock);
        assert!(prepare_data_dir(&state).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
