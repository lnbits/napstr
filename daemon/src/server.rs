use crate::{
    audio,
    library::{checked_path, Library, SharedFile},
    protocol::{read_frame, write_frame, ClientFrame, ServerFrame, PROTOCOL_VERSION},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Seek, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore},
    task::JoinSet,
    time::timeout,
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub request_id: String,
    pub file_id: String,
    pub onion: String,
    pub port: u16,
    pub capability: String,
    pub expires_at: u64,
}

struct Grant {
    file_id: String,
    requester: String,
    expires_at: u64,
}

pub struct Server {
    pub library: Arc<RwLock<Library>>,
    pub root: PathBuf,
    pub data_dir: PathBuf,
    grants: Mutex<HashMap<String, Grant>>,
}

impl Server {
    pub fn new(library: Arc<RwLock<Library>>, root: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            library,
            root,
            data_dir,
            grants: Mutex::new(HashMap::new()),
        }
    }

    pub async fn offer(
        &self,
        request_id: String,
        file_id: String,
        requester: String,
        onion: &str,
    ) -> Result<Offer, String> {
        let file = self
            .library
            .read()
            .await
            .get(&file_id)
            .cloned()
            .ok_or("requested file is not currently shared")?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            checked_path(&root, &file.path)?;
            audio::validate_audio(&file.path)
                .map(|_| ())
                .map_err(|_| "shared audio is unavailable".to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut grants = self.grants.lock().await;
        grants.retain(|_, grant| grant.expires_at > now());
        if grants.len() >= 64
            || grants
                .values()
                .any(|g| g.requester == requester && g.file_id == file_id)
        {
            return Err("private offer limit reached; try again later".into());
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let capability = hex::encode(bytes);
        let expires_at = now() + 15 * 60;
        grants.insert(
            capability_key(&capability),
            Grant {
                file_id: file_id.clone(),
                requester,
                expires_at,
            },
        );
        Ok(Offer {
            request_id,
            file_id,
            onion: onion.into(),
            port: 80,
            capability,
            expires_at,
        })
    }

    pub async fn revoke(&self, capability: &str) {
        self.grants.lock().await.remove(&capability_key(capability));
    }

    pub async fn listen(self: Arc<Self>, listener: TcpListener) -> Result<(), String> {
        let slots = Arc::new(Semaphore::new(4));
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.map_err(|e| e.to_string())?;
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue; };
                    let server = self.clone();
                    connections.spawn(async move {
                        let _slot = slot;
                        // Bound slow readers, idle peers, and scratch disk consumption.
                        let _ = timeout(Duration::from_secs(2 * 60 * 60), server.serve(stream)).await;
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(&self, mut stream: S) -> Result<(), String> {
        let hello: ClientFrame = timeout(Duration::from_secs(30), read_frame(&mut stream))
            .await
            .map_err(|_| "HELLO timed out")??;
        let (capability, file_id) = match hello {
            ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capability,
                file_id,
            } => (capability, file_id),
            _ => return send_error(&mut stream, "BAD_HELLO", "protocol negotiation failed").await,
        };
        let authorized = {
            let mut grants = self.grants.lock().await;
            let key = capability_key(&capability);
            if grants
                .get(&key)
                .is_some_and(|g| g.file_id == file_id && g.expires_at > now())
            {
                grants.remove(&key);
                true
            } else {
                false
            }
        };
        if !authorized {
            return send_error(
                &mut stream,
                "UNAUTHORIZED",
                "capability is invalid or expired",
            )
            .await;
        }
        let file = self.library.read().await.get(&file_id).cloned();
        let Some(file) = file else {
            return send_error(
                &mut stream,
                "LOCAL_FILE_UNAVAILABLE",
                "file is no longer shared",
            )
            .await;
        };
        let source = file.clone();
        let root = self.root.clone();
        let data_dir = self.data_dir.clone();
        let snapshot = tokio::task::spawn_blocking(move || snapshot(&root, &data_dir, &source))
            .await
            .map_err(|e| e.to_string())?;
        let mut snapshot = match snapshot {
            Ok(file) => tokio::fs::File::from_std(file),
            Err(_) => {
                return send_error(
                    &mut stream,
                    "LOCAL_FILE_UNAVAILABLE",
                    "shared audio changed or is unavailable",
                )
                .await
            }
        };
        write_frame(
            &mut stream,
            &ServerFrame::Welcome {
                version: PROTOCOL_VERSION,
                file_id: file_id.clone(),
                filename: file.filename,
                size: file.size,
            },
        )
        .await?;
        let mut sent = false;
        loop {
            let frame = timeout(
                Duration::from_secs(120),
                read_frame::<_, ClientFrame>(&mut stream),
            )
            .await
            .map_err(|_| "peer timed out")??;
            match frame {
                ClientFrame::RequestFile if !sent => {
                    write_frame(
                        &mut stream,
                        &ServerFrame::FileData {
                            size: file.size,
                            sha256: file_id.clone(),
                        },
                    )
                    .await?;
                    tokio::io::copy(&mut snapshot, &mut stream)
                        .await
                        .map_err(|e| e.to_string())?;
                    stream.flush().await.map_err(|e| e.to_string())?;
                    sent = true;
                }
                ClientFrame::TransferComplete if sent => {
                    write_frame(&mut stream, &ServerFrame::TransferComplete).await?;
                    return Ok(());
                }
                ClientFrame::Cancel => return Ok(()),
                _ => return Err("unexpected transfer frame".into()),
            }
        }
    }
}

fn capability_key(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

async fn send_error<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: &str,
    message: &str,
) -> Result<(), String> {
    write_frame(
        stream,
        &ServerFrame::Error {
            code: code.into(),
            message: message.into(),
        },
    )
    .await?;
    Err(message.into())
}

pub(crate) fn snapshot(
    root: &std::path::Path,
    data_dir: &std::path::Path,
    file: &SharedFile,
) -> Result<std::fs::File, String> {
    checked_path(root, &file.path)?;
    audio::validate_audio(&file.path)?;
    let mut source = std::fs::File::open(&file.path).map_err(|e| e.to_string())?;
    // An unnamed private temporary file is removed even after a crash. Never
    // stream mutable source bytes until the complete indexed hash is verified.
    let mut snapshot = tempfile::tempfile_in(data_dir).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut remaining = file.size;
    let mut buffer = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let wanted = remaining.min(buffer.len() as u64) as usize;
        let count = source
            .read(&mut buffer[..wanted])
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("shared file changed".into());
        }
        hash.update(&buffer[..count]);
        snapshot
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        remaining -= count as u64;
    }
    if source.read(&mut buffer[..1]).map_err(|e| e.to_string())? != 0
        || hex::encode(hash.finalize()) != file.file_id
    {
        return Err("shared file changed".into());
    }
    snapshot.rewind().map_err(|e| e.to_string())?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{test_wav, Scanner};
    use tokio::io::{duplex, AsyncReadExt};

    async fn fixture() -> (tempfile::TempDir, Arc<Server>, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("song.wav"), test_wav()).unwrap();
        let library = Scanner::default().scan(&root).unwrap();
        let id = library.keys().next().unwrap().clone();
        let server = Arc::new(Server::new(
            Arc::new(RwLock::new(library)),
            root.clone(),
            root,
        ));
        (dir, server, id)
    }

    #[tokio::test]
    async fn streams_desktop_protocol_and_consumes_capability() {
        let (_dir, server, id) = fixture().await;
        let offer = server
            .offer("request".into(), id.clone(), "peer".into(), "test.onion")
            .await
            .unwrap();
        let (mut client, peer) = duplex(4096);
        let service = server.clone();
        let task = tokio::spawn(async move { service.serve(peer).await });
        write_frame(
            &mut client,
            &ClientFrame::Hello {
                version: 2,
                capability: offer.capability.clone(),
                file_id: id.clone(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut client).await.unwrap(),
            ServerFrame::Welcome { .. }
        ));
        write_frame(&mut client, &ClientFrame::RequestFile)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut client).await.unwrap(),
            ServerFrame::FileData { size: 48, .. }
        ));
        let mut bytes = vec![0; 48];
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes, test_wav());
        write_frame(&mut client, &ClientFrame::TransferComplete)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut client).await.unwrap(),
            ServerFrame::TransferComplete
        ));
        task.await.unwrap().unwrap();
        let (mut client, peer) = duplex(4096);
        let task = tokio::spawn(async move { server.serve(peer).await });
        write_frame(
            &mut client,
            &ClientFrame::Hello {
                version: 2,
                capability: offer.capability,
                file_id: id,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, ServerFrame>(&mut client).await.unwrap(), ServerFrame::Error { code, .. } if code == "UNAUTHORIZED")
        );
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn refuses_changed_file_before_sending_any_audio() {
        let (dir, server, id) = fixture().await;
        let offer = server
            .offer("request".into(), id.clone(), "peer".into(), "test.onion")
            .await
            .unwrap();
        let mut changed = test_wav();
        changed[47] = 1;
        std::fs::write(dir.path().join("song.wav"), changed).unwrap();
        let (mut client, peer) = duplex(4096);
        let task = tokio::spawn(async move { server.serve(peer).await });
        write_frame(
            &mut client,
            &ClientFrame::Hello {
                version: 2,
                capability: offer.capability,
                file_id: id,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, ServerFrame>(&mut client).await.unwrap(), ServerFrame::Error { code, .. } if code == "LOCAL_FILE_UNAVAILABLE")
        );
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn rejects_expired_grants_and_symlink_replacements() {
        let (dir, server, id) = fixture().await;
        let offer = server
            .offer("request".into(), id.clone(), "peer".into(), "test.onion")
            .await
            .unwrap();
        server
            .grants
            .lock()
            .await
            .get_mut(&capability_key(&offer.capability))
            .unwrap()
            .expires_at = 0;
        let (mut client, peer) = duplex(4096);
        let service = server.clone();
        let task = tokio::spawn(async move { service.serve(peer).await });
        write_frame(
            &mut client,
            &ClientFrame::Hello {
                version: 2,
                capability: offer.capability,
                file_id: id.clone(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_frame::<_, ServerFrame>(&mut client).await.unwrap(), ServerFrame::Error { code, .. } if code == "UNAUTHORIZED")
        );
        assert!(task.await.unwrap().is_err());
        #[cfg(unix)]
        {
            let other = tempfile::tempdir().unwrap();
            std::fs::write(other.path().join("private.wav"), test_wav()).unwrap();
            std::fs::remove_file(dir.path().join("song.wav")).unwrap();
            std::os::unix::fs::symlink(
                other.path().join("private.wav"),
                dir.path().join("song.wav"),
            )
            .unwrap();
            assert!(server
                .offer("request".into(), id, "peer".into(), "test.onion")
                .await
                .is_err());
        }
    }
}
