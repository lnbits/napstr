use super::*;
use crate::{
    downloads::Downloads,
    library::{test_wav, Scanner},
    tor::TorManager,
};

async fn fixture() -> (tempfile::TempDir, Arc<MobileService>, Endpoint, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let music = root.join("music");
    let data = root.join("state");
    fs::create_dir(&music).unwrap();
    fs::create_dir(&data).unwrap();
    fs::write(music.join("song.wav"), test_wav()).unwrap();
    let library = Scanner::default().scan(&music).unwrap();
    let id = library.keys().next().unwrap().clone();
    let library = Arc::new(RwLock::new(library));
    let client = nostr_sdk::Client::new(nostr_sdk::Keys::generate());
    let tor = Arc::new(TorManager::new(data.clone(), data.clone()));
    let downloads = Downloads::new(client, tor, music.clone(), &data, library.clone()).unwrap();
    let service = MobileService::new(library, music, data.clone(), downloads).unwrap();
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(load_key(&data.join("iroh-identity")).unwrap())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    service.install_endpoint(endpoint.clone()).await;
    (dir, service, endpoint, id)
}

async fn exchange(
    connection: &iroh::endpoint::Connection,
    request: ClientRequest,
) -> Result<(ServerResponse, iroh::endpoint::RecvStream), String> {
    timeout(Duration::from_secs(5), async {
        let (mut send, mut receive) = connection.open_bi().await.map_err(|e| e.to_string())?;
        write_frame(&mut send, &request).await?;
        send.finish().map_err(|e| e.to_string())?;
        let response = read_frame(&mut receive).await?;
        Ok((response, receive))
    })
    .await
    .map_err(|_| "Iroh test request timed out".to_string())?
}

#[tokio::test]
async fn napstrfy_pairs_browses_streams_reconnects_and_loses_revoked_access() {
    let (_dir, service, host, id) = fixture().await;
    let offer = service.pair_with_endpoint(&host, true).unwrap();
    let ticket = PairingTicket::from_uri(&offer.ticket).unwrap();
    let client = Endpoint::builder(presets::Minimal).bind().await.unwrap();
    let address: iroh::EndpointAddr = serde_json::from_str(&ticket.endpoint_addr).unwrap();
    let connection = timeout(
        Duration::from_secs(5),
        client.connect(address.clone(), ALPN),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        exchange(&connection, ClientRequest::Status)
            .await
            .unwrap()
            .0,
        ServerResponse::Error { .. }
    ));
    let pair = ClientRequest::Pair {
        token: ticket.token,
        device_name: "Phone\u{202e}".into(),
    };
    assert!(matches!(
        exchange(&connection, pair.clone()).await.unwrap().0,
        ServerResponse::Paired {
            stream_only: true,
            ..
        }
    ));
    assert!(matches!(
        exchange(&connection, pair).await.unwrap().0,
        ServerResponse::Error { .. }
    ));
    let (response, _) = exchange(
        &connection,
        ClientRequest::Library {
            query: String::new(),
            offset: 0,
            limit: 200,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(response, ServerResponse::Library { tracks, total: 1 } if tracks.len() == 1 && tracks[0].file_id == id)
    );
    assert!(matches!(
        exchange(&connection, ClientRequest::Transfers)
            .await
            .unwrap()
            .0,
        ServerResponse::Error { .. }
    ));
    assert!(matches!(
        exchange(
            &connection,
            ClientRequest::RequestDownload {
                file_id: id.clone(),
                source_pubkeys: vec![],
                destination_folder: None
            }
        )
        .await
        .unwrap()
        .0,
        ServerResponse::Error { .. }
    ));
    let (response, mut audio) = exchange(
        &connection,
        ClientRequest::FetchAudio {
            file_id: id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(response, ServerResponse::AudioReady { track } if track.size == 48));
    let bytes = audio.read_to_end(1024).await.unwrap();
    assert_eq!(bytes, test_wav());
    assert!(
        matches!(exchange(&connection, ClientRequest::Available { file_ids: vec![id] }).await.unwrap().0, ServerResponse::Available { file_ids } if file_ids.len() == 1)
    );
    connection.close(0u32.into(), b"reconnect");
    let connection = client.connect(address, ALPN).await.unwrap();
    assert!(matches!(
        exchange(&connection, ClientRequest::Status)
            .await
            .unwrap()
            .0,
        ServerResponse::Status {
            stream_only: true,
            ..
        }
    ));
    // Re-pairing the same authenticated endpoint changes its permission.
    let full = service.pair_with_endpoint(&host, false).unwrap();
    let full = PairingTicket::from_uri(&full.ticket).unwrap();
    assert!(matches!(
        exchange(
            &connection,
            ClientRequest::Pair {
                token: full.token,
                device_name: "Owner".into()
            }
        )
        .await
        .unwrap()
        .0,
        ServerResponse::Paired {
            stream_only: false,
            ..
        }
    ));
    assert!(
        matches!(exchange(&connection, ClientRequest::Transfers).await.unwrap().0, ServerResponse::Transfers { transfers } if transfers.is_empty())
    );
    let loaded = Access::load(service.data.join("iroh-devices.json")).unwrap();
    assert!(!loaded.devices[&client.id().to_string()].stream_only);
    assert_eq!(
        load_key(&service.data.join("iroh-identity"))
            .unwrap()
            .public(),
        host.id()
    );
    service.revoke(&client.id().to_string()).unwrap();
    if let Ok((response, _)) = exchange(&connection, ClientRequest::Status).await {
        assert!(matches!(response, ServerResponse::Error { .. }));
    }
    assert!(Access::load(service.data.join("iroh-devices.json"))
        .unwrap()
        .devices
        .is_empty());
    service.stop().await;
    client.close().await;
}

#[tokio::test]
async fn expired_and_superseded_tickets_cannot_pair() {
    let (_dir, service, host, _) = fixture().await;
    let old =
        PairingTicket::from_uri(&service.pair_with_endpoint(&host, true).unwrap().ticket).unwrap();
    let current =
        PairingTicket::from_uri(&service.pair_with_endpoint(&host, true).unwrap().ticket).unwrap();
    let full =
        PairingTicket::from_uri(&service.pair_with_endpoint(&host, false).unwrap().ticket).unwrap();
    let remote = SecretKey::generate().public().to_string();
    {
        let mut access = service.access.lock().unwrap();
        assert!(access.accept(&remote, &old.token, "Phone").is_err());
        assert!(access.accept(&remote, &current.token, "Phone").unwrap());
        access
            .pending
            .iter_mut()
            .find(|s| s.token == full.token)
            .unwrap()
            .expires_at = now() - 1;
        assert!(access.accept(&remote, &full.token, "Phone").is_err());
    }
    service.stop().await;
}

#[tokio::test]
async fn audiobook_drop_zone_is_separate_and_ordered_like_desktop() {
    let (_dir, service, _host, _) = fixture().await;
    let book = service.root.join("Audiobooks/A_Book");
    fs::create_dir_all(&book).unwrap();
    for (name, sample) in [("Chapter 10.wav", 10), ("Chapter 2.wav", 2)] {
        let mut audio = test_wav();
        audio[47] = sample;
        fs::write(book.join(name), audio).unwrap();
    }
    *service.library.write().await = Scanner::default().scan(&service.root).unwrap();
    service.library_changed();
    let catalogue = service.catalogue().await;
    assert_eq!(catalogue.tracks.len(), 1);
    assert_eq!(catalogue.books.len(), 1);
    assert_eq!(catalogue.books[0].title, "A Book");
    assert_eq!(catalogue.books[0].chapters[0].filename, "Chapter 2.wav");
    assert_eq!(catalogue.books[0].chapters[1].filename, "Chapter 10.wav");
    service.stop().await;
}

#[tokio::test]
async fn rejects_oversized_control_frames_before_allocation() {
    let (mut send, mut receive) = tokio::io::duplex(4);
    send.write_u32((MAX_CONTROL_FRAME_BYTES + 1) as u32)
        .await
        .unwrap();
    assert!(read_frame::<_, ClientRequest>(&mut receive).await.is_err());
}
