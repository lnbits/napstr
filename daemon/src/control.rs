//! Local administration only: the socket lives in the daemon's private state directory.
use crate::mobile::{self, Device, MobileService, PairingOffer};
use qrcode::{
    render::{svg, unicode},
    QrCode,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

#[derive(Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Request {
    Pair { read_only: bool },
    Devices,
    Revoke { endpoint_id: String },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Response {
    Pairing(PairingOffer),
    Devices { devices: Vec<Device> },
    Revoked,
    Error { message: String },
}

pub struct Command {
    request: Request,
    json: bool,
    qr_file: Option<PathBuf>,
}

impl Command {
    pub fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let Some(verb) = args.first().map(String::as_str) else {
            return Ok(None);
        };
        if !matches!(verb, "pair" | "devices" | "revoke") {
            return Ok(None);
        }
        let mut read_only = false;
        let mut json = false;
        let mut qr_file = None;
        let mut endpoint_id = None;
        let mut args = args.iter().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--read-only" if verb == "pair" => read_only = true,
                "--json" => json = true,
                "--qr-file" if verb == "pair" && qr_file.is_none() => {
                    qr_file = Some(PathBuf::from(
                        args.next().ok_or("--qr-file requires an output path")?,
                    ))
                }
                id if verb == "revoke" && endpoint_id.is_none() => {
                    endpoint_id = Some(
                        id.parse::<iroh::EndpointId>()
                            .map_err(|_| "revoke requires a valid Iroh endpoint ID")?
                            .to_string(),
                    );
                }
                _ => return Err("Invalid administration arguments; use --help".into()),
            }
        }
        let request = match verb {
            "pair" => Request::Pair { read_only },
            "devices" => Request::Devices,
            "revoke" => Request::Revoke {
                endpoint_id: endpoint_id.ok_or("revoke requires an Iroh endpoint ID")?,
            },
            _ => unreachable!(),
        };
        Ok(Some(Self {
            request,
            json,
            qr_file,
        }))
    }

    pub async fn run(self) -> Result<(), String> {
        let data = PathBuf::from(
            std::env::var_os("NAPSTR_DATA_DIR").unwrap_or_else(|| "/var/lib/napstrd".into()),
        );
        if !data.is_absolute() {
            return Err("NAPSTR_DATA_DIR must be absolute".into());
        }
        let mut socket = UnixStream::connect(data.join("control.sock")).await.map_err(|e| format!("Cannot contact napstrd: {e}. Start the service and run this command as its user (or root), with the same NAPSTR_DATA_DIR."))?;
        mobile::write_frame(&mut socket, &self.request).await?;
        let response: Response = timeout(Duration::from_secs(30), mobile::read_frame(&mut socket))
            .await
            .map_err(|_| "napstrd administration request timed out")??;
        if let Response::Error { message } = response {
            return Err(message);
        }
        if let Response::Pairing(offer) = &response {
            let qr = QrCode::new(offer.ticket.as_bytes()).map_err(|e| e.to_string())?;
            if let Some(path) = &self.qr_file {
                let svg = qr
                    .render::<svg::Color>()
                    .min_dimensions(280, 280)
                    .dark_color(svg::Color("#000000"))
                    .light_color(svg::Color("#ffffff"))
                    .build();
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .map_err(|e| format!("Cannot create pairing QR file: {e}"))?;
                file.write_all(svg.as_bytes()).map_err(|e| e.to_string())?;
                eprintln!("Pairing QR saved to {}", path.display());
            }
            if !self.json {
                println!(
                    "{}",
                    qr.render::<unicode::Dense1x2>()
                        .quiet_zone(true)
                        .dark_color(unicode::Dense1x2::Light)
                        .light_color(unicode::Dense1x2::Dark)
                        .build()
                );
                println!("{}", offer.ticket);
                eprintln!("Scan this code or paste the URI into Napstrfy. It expires in five minutes and can be used once.");
            }
        } else if !self.json {
            match &response {
                Response::Devices { devices } if devices.is_empty() => {
                    println!("No paired devices")
                }
                Response::Devices { devices } => {
                    for device in devices {
                        println!(
                            "{}\t{}\t{}",
                            device.endpoint_id,
                            if device.stream_only {
                                "read-only"
                            } else {
                                "full-access"
                            },
                            device.name
                        );
                    }
                }
                Response::Revoked => println!("Device access revoked"),
                _ => {}
            }
        }
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&response).map_err(|e| e.to_string())?
            );
        }
        Ok(())
    }
}

pub struct ControlSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl ControlSocket {
    // Call only after taking the daemon state lock. A regular file at the
    // socket path is never removed, and no network administration port exists.
    pub fn bind(data: &std::path::Path) -> Result<Self, String> {
        let path = data.join("control.sock");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                fs::remove_file(&path).map_err(|e| e.to_string())?
            }
            Ok(_) => return Err("control.sock exists and is not a Unix socket".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| format!("Cannot create administration socket: {e}"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        Ok(Self { listener, path })
    }

    pub async fn serve(self, service: Arc<MobileService>) -> Result<(), String> {
        let slots = Arc::new(Semaphore::new(4));
        let mut requests = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (mut socket, _) = accepted.map_err(|e| e.to_string())?;
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    let service = service.clone();
                    requests.spawn(async move {
                        let _permit = permit;
                        let request = timeout(Duration::from_secs(5), mobile::read_frame::<_, Request>(&mut socket)).await;
                        let result = match request {
                            Ok(Ok(Request::Pair { read_only })) => service.pair(read_only).await.map(Response::Pairing),
                            Ok(Ok(Request::Devices)) => service.devices().map(|devices| Response::Devices { devices }),
                            Ok(Ok(Request::Revoke { endpoint_id })) => service.revoke(&endpoint_id).map(|_| Response::Revoked),
                            _ => Err("Invalid administration request".into()),
                        };
                        let response = result.unwrap_or_else(|message| Response::Error { message });
                        let _ = mobile::write_frame(&mut socket, &response).await;
                    });
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
            }
        }
    }
}
