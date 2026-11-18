use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    sync::{Mutex, RwLock},
    time::{sleep, timeout},
};
use tokio_socks::tcp::Socks5Stream;
use tokio_util::sync::CancellationToken;

struct RunningTor {
    generation: uuid::Uuid,
    child: Child,
    socks_port: u16,
    control_port: u16,
    cookie: Vec<u8>,
    data_dir: PathBuf,
}

pub struct OnionLease {
    pub onion: String,
    generation: uuid::Uuid,
    target_port: u16,
    control: Mutex<TcpStream>,
}

pub struct TorManager {
    app_data: PathBuf,
    resource_dir: PathBuf,
    runtime: Mutex<Option<RunningTor>>,
    starting: AtomicBool,
    cancel_start: AtomicBool,
    bootstrap_progress: AtomicU8,
    last_error: RwLock<String>,
    last_diagnostics: Arc<RwLock<Vec<String>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TorStatus {
    pub running: bool,
    pub starting: bool,
    pub bootstrap_progress: u8,
    pub error: String,
}

pub fn is_v3_onion(host: &str) -> bool {
    host.strip_suffix(".onion")
        .map(|service| {
            service.len() == 56
                && service
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
        })
        .unwrap_or(false)
}

impl TorManager {
    pub fn new(app_data: PathBuf, resource_dir: PathBuf) -> Self {
        Self {
            app_data,
            resource_dir,
            runtime: Mutex::new(None),
            starting: AtomicBool::new(false),
            cancel_start: AtomicBool::new(false),
            bootstrap_progress: AtomicU8::new(0),
            last_error: RwLock::new(String::new()),
            last_diagnostics: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn start(&self) -> Result<u16, String> {
        let mut guard = self.runtime.lock().await;
        self.cancel_start.store(false, Ordering::SeqCst);
        if let Some(runtime) = guard.as_mut() {
            if runtime
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(runtime.socks_port);
            }
        }
        *guard = None;
        self.starting.store(true, Ordering::SeqCst);
        self.bootstrap_progress.store(0, Ordering::SeqCst);
        self.last_error.write().await.clear();
        self.last_diagnostics.write().await.clear();

        let tor_data = self
            .app_data
            .join("tor-sessions")
            .join(uuid::Uuid::new_v4().to_string());
        let cleanup_data = tor_data.clone();
        let result: Result<u16, String> = async {
            let tor = self.find_tor_binary();
            fs::create_dir_all(&tor_data)
                .await
                .map_err(|error| error.to_string())?;
            let cookie_path = tor_data.join("control_auth_cookie");
            let control_port_path = tor_data.join("control-port");
            let _ = fs::remove_file(&cookie_path).await;
            let _ = fs::remove_file(&control_port_path).await;

            let mut command = Command::new(&tor);
            command
                .arg("--DataDirectory")
                .arg(&tor_data)
                .arg("--SocksPort")
                .arg("auto")
                .arg("--ControlPort")
                .arg("auto")
                .arg("--ControlPortWriteToFile")
                .arg(&control_port_path)
                .arg("--CookieAuthentication")
                .arg("1")
                .arg("--CookieAuthFile")
                .arg(&cookie_path)
                .arg("--ClientOnly")
                .arg("1")
                .arg("--AvoidDiskWrites")
                .arg("1")
                .arg("--Log")
                .arg("notice stdout")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if let Some(parent) = tor.parent() {
                prepend_library_path(&mut command, parent);
            }
            hide_child_process_window(&mut command);
            command.kill_on_drop(true);

            let mut child = command
                .spawn()
                .map_err(|error| format!("could not start Tor at {}: {error}", tor.display()))?;
            if let Some(stdout) = child.stdout.take() {
                capture_process_output(stdout, self.last_diagnostics.clone());
            }
            if let Some(stderr) = child.stderr.take() {
                capture_process_output(stderr, self.last_diagnostics.clone());
            }
            let mut ready = None;
            for _ in 0..480 {
                if self.cancel_start.load(Ordering::SeqCst) {
                    let _ = child.start_kill();
                    return Err("Tor startup was cancelled for network recovery".into());
                }
                if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                    sleep(Duration::from_millis(50)).await;
                    let diagnostic = useful_diagnostic(&self.last_diagnostics.read().await);
                    let detail = if diagnostic.is_empty() {
                        String::new()
                    } else {
                        format!(": {diagnostic}")
                    };
                    return Err(format!(
                        "Tor exited before bootstrap completed ({status}){detail}"
                    ));
                }
                if let (Ok(bytes), Ok(control_address)) = (
                    fs::read(&cookie_path).await,
                    fs::read_to_string(&control_port_path).await,
                ) {
                    let Some(control_port) = listener_port(&control_address) else {
                        sleep(Duration::from_millis(250)).await;
                        continue;
                    };
                    if TcpStream::connect(("127.0.0.1", control_port))
                        .await
                        .is_ok()
                    {
                        let mut control = TcpStream::connect(("127.0.0.1", control_port))
                            .await
                            .map_err(|error| error.to_string())?;
                        if control_command(
                            &mut control,
                            &format!("AUTHENTICATE {}", hex::encode(&bytes)),
                        )
                        .await
                        .is_ok()
                        {
                            let socks_port =
                                control_command(&mut control, "GETINFO net/listeners/socks")
                                    .await
                                    .ok()
                                    .and_then(|lines| listener_port_from_control(&lines));
                            if let Ok(lines) =
                                control_command(&mut control, "GETINFO status/bootstrap-phase")
                                    .await
                            {
                                if let Some(progress) = bootstrap_progress(&lines) {
                                    self.bootstrap_progress.store(progress, Ordering::SeqCst);
                                    if progress == 100 && socks_port.is_some() {
                                        ready = Some((bytes, control_port, socks_port.unwrap()));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                sleep(Duration::from_millis(250)).await;
            }
            let (cookie, control_port, socks_port) = ready
                .ok_or_else(|| "Tor did not complete bootstrap within 120 seconds".to_string())?;
            *guard = Some(RunningTor {
                generation: uuid::Uuid::new_v4(),
                child,
                socks_port,
                control_port,
                cookie,
                data_dir: tor_data,
            });
            Ok(socks_port)
        }
        .await;

        if result.is_err() {
            let _ = fs::remove_dir_all(cleanup_data).await;
        }

        self.starting.store(false, Ordering::SeqCst);
        match &result {
            Ok(_) => {
                self.bootstrap_progress.store(100, Ordering::SeqCst);
                self.last_error.write().await.clear();
            }
            Err(error) => *self.last_error.write().await = error.clone(),
        }
        result
    }

    pub async fn stop(&self) {
        self.cancel_start.store(true, Ordering::SeqCst);
        if let Some(mut runtime) = self.runtime.lock().await.take() {
            let _ = runtime.child.start_kill();
            let _ = timeout(Duration::from_secs(5), runtime.child.wait()).await;
            let _ = fs::remove_dir_all(runtime.data_dir).await;
        }
    }

    pub async fn restart(&self) -> Result<u16, String> {
        // Keep status in "starting" throughout the handover so the regular
        // health poll cannot launch a competing Tor process between stop/start.
        self.starting.store(true, Ordering::SeqCst);
        self.cancel_start.store(true, Ordering::SeqCst);
        self.stop().await;
        self.start().await
    }

    pub async fn status(&self) -> TorStatus {
        let running = self
            .runtime
            .try_lock()
            .ok()
            .and_then(|mut guard| {
                guard
                    .as_mut()
                    .map(|runtime| runtime.child.try_wait().ok().flatten().is_none())
            })
            .unwrap_or(false);
        TorStatus {
            running,
            starting: self.starting.load(Ordering::SeqCst),
            bootstrap_progress: self.bootstrap_progress.load(Ordering::SeqCst),
            error: self.last_error.read().await.clone(),
        }
    }

    pub async fn create_onion(&self, target_port: u16) -> Result<Arc<OnionLease>, String> {
        self.ensure_onion(target_port, None).await
    }

    pub async fn ensure_onion(
        &self,
        target_port: u16,
        cached: Option<&Arc<OnionLease>>,
    ) -> Result<Arc<OnionLease>, String> {
        self.start().await?;
        // Keep stop/restart from replacing the runtime during the control check
        // or ADD_ONION. Every newly bootstrapped process gets a fresh generation.
        let guard = self.runtime.lock().await;
        let runtime = guard.as_ref().ok_or("Tor runtime disappeared")?;
        ensure_runtime_onion(
            runtime.generation,
            runtime.control_port,
            &runtime.cookie,
            target_port,
            cached,
        )
        .await
    }

    pub async fn connect_onion(
        &self,
        onion: &str,
        port: u16,
    ) -> Result<Socks5Stream<TcpStream>, String> {
        if !is_v3_onion(onion) {
            return Err("refusing a destination that is not a valid Tor v3 onion".into());
        }
        let socks_port = self.start().await?;
        timeout(
            Duration::from_secs(20),
            Socks5Stream::connect(("127.0.0.1", socks_port), (onion, port)),
        )
        .await
        .map_err(|_| "Tor connection attempt timed out".to_string())?
        .map_err(|error| format!("Tor connection failed: {error}"))
    }

    pub async fn connect_onion_with_retry(
        &self,
        onion: &str,
        port: u16,
        cancel: &CancellationToken,
    ) -> Result<Socks5Stream<TcpStream>, String> {
        let mut last_error = String::new();
        for attempt in 0..8 {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match self.connect_onion(onion, port).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error,
            }
            let delay = Duration::from_secs((attempt + 1).min(5));
            tokio::select! {
                _ = cancel.cancelled() => return Err("cancelled".into()),
                _ = sleep(delay) => {}
            }
        }
        Err(format!(
            "temporary onion service was not reachable: {last_error}"
        ))
    }

    fn find_tor_binary(&self) -> PathBuf {
        if let Ok(value) = std::env::var("NAPSTR_TOR_PATH") {
            return PathBuf::from(value);
        }
        let executable = if cfg!(windows) { "tor.exe" } else { "tor" };
        let platform = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        for candidate in [
            self.resource_dir
                .join("resources")
                .join("tor")
                .join(platform)
                .join("tor")
                .join(executable),
            self.resource_dir
                .join("resources")
                .join("tor")
                .join(platform)
                .join(executable),
            self.resource_dir
                .join("tor")
                .join(platform)
                .join("tor")
                .join(executable),
            self.resource_dir
                .join("tor")
                .join(platform)
                .join(executable),
            self.resource_dir.join("tor").join(executable),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
        PathBuf::from(executable)
    }
}

#[cfg(target_os = "windows")]
fn hide_child_process_window(command: &mut Command) {
    // Tor is a console executable. Napstr captures its output through pipes, so
    // it does not need a visible terminal window when launched by the GUI app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_child_process_window(_command: &mut Command) {}

fn capture_process_output<R>(stream: R, destination: Arc<RwLock<Vec<String>>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if !line.is_empty() {
                let mut diagnostics = destination.write().await;
                diagnostics.push(line.chars().take(500).collect());
                if diagnostics.len() > 16 {
                    diagnostics.remove(0);
                }
            }
        }
    });
}

fn useful_diagnostic(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .find(|line| line.contains("[warn]") && !line.contains("Fixing permissions on directory"))
        .or_else(|| {
            lines.iter().rev().find(|line| {
                line.contains("[err]")
                    && !line.contains("Reading config failed--see warnings above")
                    && !line.contains("set_options(): Bug:")
            })
        })
        .or_else(|| lines.last())
        .cloned()
        .unwrap_or_default()
}

fn bootstrap_progress(lines: &[String]) -> Option<u8> {
    lines.iter().find_map(|line| {
        line.split_whitespace().find_map(|field| {
            field
                .strip_prefix("PROGRESS=")
                .and_then(|value| value.parse::<u8>().ok())
                .filter(|value| *value <= 100)
        })
    })
}

async fn ensure_runtime_onion(
    generation: uuid::Uuid,
    control_port: u16,
    cookie: &[u8],
    target_port: u16,
    cached: Option<&Arc<OnionLease>>,
) -> Result<Arc<OnionLease>, String> {
    if let Some(lease) = cached.filter(|lease| {
        lease.generation == generation && lease.target_port == target_port
    }) {
        // This connection owns the ephemeral service. A live process alone is
        // insufficient: losing the control connection also deletes the onion.
        let alive = timeout(Duration::from_secs(3), async {
            let mut control = lease.control.lock().await;
            control_command(&mut control, "GETINFO version").await
        })
        .await;
        if matches!(alive, Ok(Ok(_))) {
            return Ok(lease.clone());
        }
    }

    timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(("127.0.0.1", control_port))
            .await
            .map_err(|error| error.to_string())?;
        control_command(&mut stream, &format!("AUTHENTICATE {}", hex::encode(cookie))).await?;
        let response = control_command(
            &mut stream,
            &format!("ADD_ONION NEW:BEST Flags=DiscardPK Port=80,127.0.0.1:{target_port}"),
        )
        .await?;
        let service_id = response
            .iter()
            .find_map(|line| line.strip_prefix("250-ServiceID="))
            .ok_or("Tor did not return a ServiceID")?;
        let onion = format!("{service_id}.onion");
        if !is_v3_onion(&onion) {
            return Err("Tor returned an invalid onion service address".into());
        }
        Ok(Arc::new(OnionLease {
            onion,
            generation,
            target_port,
            control: Mutex::new(stream),
        }))
    })
    .await
    .map_err(|_| "Tor onion service creation timed out".to_string())?
}

async fn control_command(stream: &mut TcpStream, command: &str) -> Result<Vec<String>, String> {
    stream
        .write_all(format!("{command}\r\n").as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("Tor control connection closed unexpectedly".into());
        }
        let line = line.trim_end().to_string();
        if line.starts_with("250") {
            let complete =
                line == "250 OK" || (line.starts_with("250 ") && !line.starts_with("250-"));
            lines.push(line);
            if complete {
                return Ok(lines);
            }
        } else if line.len() >= 3 {
            return Err(format!("Tor control error: {line}"));
        }
    }
}

fn listener_port(value: &str) -> Option<u16> {
    value
        .trim()
        .trim_matches('"')
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim_matches('"').parse().ok())
}

fn listener_port_from_control(lines: &[String]) -> Option<u16> {
    lines.iter().find_map(|line| {
        line.strip_prefix("250-net/listeners/socks=")
            .or_else(|| line.strip_prefix("250 net/listeners/socks="))
            .and_then(|listeners| listeners.split_whitespace().find_map(listener_port))
    })
}

#[cfg(target_os = "linux")]
fn prepend_library_path(command: &mut Command, directory: &Path) {
    let value = std::env::var_os("LD_LIBRARY_PATH")
        .map(|existing| format!("{}:{}", directory.display(), existing.to_string_lossy()))
        .unwrap_or_else(|| directory.display().to_string());
    command.env("LD_LIBRARY_PATH", value);
}

#[cfg(target_os = "macos")]
fn prepend_library_path(command: &mut Command, directory: &Path) {
    let value = std::env::var_os("DYLD_LIBRARY_PATH")
        .map(|existing| format!("{}:{}", directory.display(), existing.to_string_lossy()))
        .unwrap_or_else(|| directory.display().to_string());
    command.env("DYLD_LIBRARY_PATH", value);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn prepend_library_path(_command: &mut Command, _directory: &Path) {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    // A local control-protocol fixture exercises connection ownership without
    // a Tor binary, public network access, or real keys.
    pub(crate) struct TestControl {
        port: u16,
        created: Arc<std::sync::atomic::AtomicUsize>,
        reject_creation: Arc<AtomicBool>,
        stall_probe: Arc<AtomicBool>,
        connections: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
        listener: tokio::task::JoinHandle<()>,
    }

    impl TestControl {
        pub(crate) async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reject_creation = Arc::new(AtomicBool::new(false));
            let stall_probe = Arc::new(AtomicBool::new(false));
            let connections = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (count, reject, stall, clients) = (
                created.clone(), reject_creation.clone(), stall_probe.clone(), connections.clone(),
            );
            let listener = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (count, reject, stall) = (count.clone(), reject.clone(), stall.clone());
                    let task = tokio::spawn(async move {
                        let mut stream = BufReader::new(stream);
                        loop {
                            let mut command = String::new();
                            if !matches!(stream.read_line(&mut command).await, Ok(n) if n > 0) {
                                break;
                            }
                            let response = if command.starts_with("AUTHENTICATE ") {
                                "250 OK\r\n".to_string()
                            } else if command.starts_with("ADD_ONION ") {
                                if reject.load(Ordering::SeqCst) {
                                    "551 creation failed\r\n".to_string()
                                } else {
                                    let n = count.fetch_add(1, Ordering::SeqCst);
                                    let id = char::from(b'a' + n as u8).to_string().repeat(56);
                                    format!("250-ServiceID={id}\r\n250 OK\r\n")
                                }
                            } else if command.trim() == "GETINFO version" {
                                if stall.load(Ordering::SeqCst) { continue; }
                                "250-version=test\r\n250 OK\r\n".to_string()
                            } else {
                                panic!("unexpected test control command");
                            };
                            if stream.get_mut().write_all(response.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    });
                    clients.lock().unwrap().push(task.abort_handle());
                }
            });
            Self { port, created, reject_creation, stall_probe, connections, listener }
        }

        fn disconnect_services(&self) {
            for connection in self.connections.lock().unwrap().drain(..) {
                connection.abort();
            }
        }

        pub(crate) async fn lease(&self, generation: uuid::Uuid, cached: Option<&Arc<OnionLease>>) -> Result<Arc<OnionLease>, String> {
            ensure_runtime_onion(generation, self.port, b"test-cookie", 12345, cached).await
        }
    }

    impl Drop for TestControl {
        fn drop(&mut self) {
            self.listener.abort();
            self.disconnect_services();
        }
    }

    #[tokio::test]
    async fn onion_cache_reuses_live_service_but_not_a_previous_tor_generation() {
        let control = TestControl::start().await;
        let generation = uuid::Uuid::new_v4();
        let first = control.lease(generation, None).await.unwrap();
        let reused = control.lease(generation, Some(&first)).await.unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        assert_eq!(control.created.load(Ordering::SeqCst), 1);
        // Even if an old TCP socket still appears live, it belongs to the old
        // process. Reusing a control port must not make its onion valid again.
        let restarted = control.lease(uuid::Uuid::new_v4(), Some(&first)).await.unwrap();
        assert_ne!(first.onion, restarted.onion);
        assert!(!Arc::ptr_eq(&first, &restarted));
        assert_eq!(control.created.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn onion_cache_replaces_a_lost_control_connection() {
        let control = TestControl::start().await;
        let generation = uuid::Uuid::new_v4();
        let first = control.lease(generation, None).await.unwrap();
        control.disconnect_services();
        let replacement = control.lease(generation, Some(&first)).await.unwrap();
        assert_ne!(first.onion, replacement.onion);
        let reused = control.lease(generation, Some(&replacement)).await.unwrap();
        assert!(Arc::ptr_eq(&replacement, &reused));
        assert_eq!(control.created.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn onion_cache_never_falls_back_to_a_dead_service_when_creation_fails() {
        let control = TestControl::start().await;
        let generation = uuid::Uuid::new_v4();
        let first = control.lease(generation, None).await.unwrap();
        control.disconnect_services();
        control.reject_creation.store(true, Ordering::SeqCst);
        assert!(control.lease(generation, Some(&first)).await.is_err());
        control.reject_creation.store(false, Ordering::SeqCst);
        let replacement = control.lease(generation, Some(&first)).await.unwrap();
        assert_ne!(first.onion, replacement.onion);
    }

    #[tokio::test]
    async fn onion_cache_probe_has_a_deadline() {
        let control = TestControl::start().await;
        let generation = uuid::Uuid::new_v4();
        let first = control.lease(generation, None).await.unwrap();
        control.stall_probe.store(true, Ordering::SeqCst);
        let replacement = timeout(Duration::from_secs(6), control.lease(generation, Some(&first)))
            .await.unwrap().unwrap();
        assert_ne!(first.onion, replacement.onion);
    }

    #[test]
    fn accepts_only_v3_onion_hostnames() {
        assert!(is_v3_onion(&format!("{}.onion", "a".repeat(56))));
        assert!(!is_v3_onion("example.com"));
        assert!(!is_v3_onion("short.onion"));
        assert!(!is_v3_onion(&format!("{}.onion.example", "a".repeat(56))));
    }

    #[tokio::test]
    async fn rejects_ssrf_destinations_before_starting_tor() {
        let directory = std::env::temp_dir().join(format!("napstr-ssrf-test-{}", uuid::Uuid::new_v4()));
        let tor = TorManager::new(directory.clone(), directory.clone());
        let onion = format!("{}.onion", "a".repeat(56));
        let destinations = [
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "10.0.0.1".to_string(),
            "192.168.1.1".to_string(),
            "169.254.169.254".to_string(),
            "::1".to_string(),
            "[::ffff:127.0.0.1]".to_string(),
            "2130706433".to_string(),
            "0x7f000001".to_string(),
            "example.com".to_string(),
            format!("http://{onion}"),
            format!("{onion}.example.com"),
            format!("{onion}@127.0.0.1"),
            format!("127.0.0.1@{onion}"),
            format!("{onion}:80"),
            format!("{onion}/path"),
            format!("{onion}.exit"),
            format!("{onion}\0.example.com"),
            format!("{onion}\r\nGET / HTTP/1.1"),
            format!("{}.onion", "a".repeat(55)),
            format!("{}.onion", "0".repeat(56)),
        ];
        for destination in destinations {
            let result = timeout(Duration::from_secs(1), tor.connect_onion(&destination, 80))
                .await
                .expect("invalid destination reached Tor startup");
            assert!(matches!(result, Err(ref error) if error ==
                "refusing a destination that is not a valid Tor v3 onion"),
                "destination was not rejected: {destination:?}");
        }
        assert!(tor.runtime.lock().await.is_none());
        assert!(!directory.exists());
    }

    #[test]
    fn reads_control_port_bootstrap_progress() {
        assert_eq!(
            bootstrap_progress(&[
                "250-status/bootstrap-phase=NOTICE BOOTSTRAP PROGRESS=75 TAG=enough_dirinfo".into()
            ]),
            Some(75)
        );
        assert_eq!(bootstrap_progress(&["250 OK".into()]), None);
    }

    #[test]
    fn keeps_the_actionable_tor_warning() {
        assert_eq!(
            useful_diagnostic(&[
                "Aug 21 14:33:17 [warn] Failed to lock data directory: another Tor process is running".into(),
                "Aug 21 14:33:22 [err] set_options(): Bug: Acting on config options left us in a broken state. Dying.".into(),
                "Aug 21 14:33:17 [err] Reading config failed--see warnings above.".into(),
            ]),
            "Aug 21 14:33:17 [warn] Failed to lock data directory: another Tor process is running"
        );
    }

    #[test]
    fn reads_tor_selected_listener_ports() {
        assert_eq!(listener_port("PORT=127.0.0.1:49152\n"), Some(49152));
        assert_eq!(
            listener_port_from_control(&[
                "250-net/listeners/socks=\"127.0.0.1:49153\"".into(),
                "250 OK".into(),
            ]),
            Some(49153)
        );
    }

    #[tokio::test]
    #[ignore = "requires a Tor binary and external Tor network access"]
    async fn concurrent_instances_do_not_share_a_tor_data_directory() {
        let directory =
            std::env::temp_dir().join(format!("napstr-tor-concurrent-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).await.unwrap();
        let first = TorManager::new(directory.clone(), directory.clone());
        let second = TorManager::new(directory.clone(), directory.clone());
        let (first_port, second_port) = tokio::join!(first.start(), second.start());
        assert_ne!(first_port.unwrap(), second_port.unwrap());
        first.stop().await;
        second.stop().await;
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    #[ignore = "requires a Tor binary and external Tor network access"]
    async fn ephemeral_onion_round_trip() {
        let directory =
            std::env::temp_dir().join(format!("napstr-tor-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).await.unwrap();
        let manager = TorManager::new(directory.clone(), directory.clone());
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let lease = manager.create_onion(port).await.unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        let cancel = CancellationToken::new();
        let mut client = manager
            .connect_onion_with_retry(&lease.onion, 80, &cancel)
            .await
            .unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = [0u8; 4];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        server.await.unwrap();
        drop(lease);
        manager.stop().await;
        let _ = fs::remove_dir_all(directory).await;
    }
}
