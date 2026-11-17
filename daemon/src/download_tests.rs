use super::*;
use crate::library::test_wav;

fn fixture() -> (tempfile::TempDir, Arc<Downloads>, Job) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let music = root.join("music");
    let data = root.join("state");
    fs::create_dir(&music).unwrap();
    fs::create_dir(&data).unwrap();
    let client = Client::new(Keys::generate());
    let service = Downloads::new(
        client,
        Arc::new(TorManager::new(data.clone(), data.clone())),
        music,
        &data,
        Arc::new(RwLock::new(Library::new())),
    )
    .unwrap();
    let track = RemoteTrack {
        file_id: hex::encode(Sha256::digest(test_wav())),
        filename: "song.wav".into(),
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        format: "WAV".into(),
        mime: "audio/wav".into(),
        size: 48,
        tags: String::new(),
        local: false,
        sources: vec![],
    };
    let job = Job {
        id: "job".into(),
        sequence: 1,
        request_id: "request".into(),
        track,
        sources: vec![],
        destination: None,
        progress: 0.0,
        status: "Queued".into(),
    };
    (dir, service, job)
}

#[tokio::test]
async fn verified_downloads_never_overwrite_existing_files() {
    let (_dir, service, job) = fixture();
    fs::write(service.root.join("song.wav"), b"keep existing").unwrap();
    let offer = Offer {
        request_id: job.request_id.clone(),
        file_id: job.track.file_id.clone(),
        onion: format!("{}.onion", "a".repeat(56)),
        port: 80,
        capability: "b".repeat(64),
        expires_at: now() + 900,
    };
    let (mut client, mut server) = tokio::io::duplex(4096);
    let id = job.track.file_id.clone();
    let peer = tokio::spawn(async move {
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut server).await.unwrap(),
            ClientFrame::Hello { .. }
        ));
        write_frame(
            &mut server,
            &ServerFrame::Welcome {
                version: 2,
                file_id: id.clone(),
                filename: "../../ignored.wav".into(),
                size: 48,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut server).await.unwrap(),
            ClientFrame::RequestFile
        ));
        write_frame(
            &mut server,
            &ServerFrame::FileData {
                size: 48,
                sha256: id,
            },
        )
        .await
        .unwrap();
        server.write_all(&test_wav()).await.unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut server).await.unwrap(),
            ClientFrame::TransferComplete
        ));
        write_frame(&mut server, &ServerFrame::TransferComplete)
            .await
            .unwrap();
    });
    service
        .download_stream(&job, &offer, &mut client)
        .await
        .unwrap();
    peer.await.unwrap();
    assert_eq!(
        fs::read(service.root.join("song.wav")).unwrap(),
        b"keep existing"
    );
    assert_eq!(
        fs::read(service.root.join("song (1).wav")).unwrap(),
        test_wav()
    );
    assert_eq!(fs::read_dir(&service.root).unwrap().count(), 2);
}

#[tokio::test]
async fn corrupted_downloads_are_removed_and_offers_are_bound_to_the_requested_source() {
    let (_dir, service, job) = fixture();
    let peer = Keys::generate();
    let stranger = Keys::generate();
    let (tx, mut rx) = mpsc::channel(3);
    service.pending.lock().unwrap().insert(
        job.request_id.clone(),
        Pending {
            file_id: job.track.file_id.clone(),
            sources: HashSet::from([peer.public_key().to_hex()]),
            offers: tx,
        },
    );
    let offer = || Offer {
        request_id: job.request_id.clone(),
        file_id: job.track.file_id.clone(),
        onion: format!("{}.onion", "a".repeat(56)),
        port: 80,
        capability: "b".repeat(64),
        expires_at: now() + 900,
    };
    service.receive_offer(stranger.public_key(), offer());
    assert!(rx.try_recv().is_err());
    let mut invalid = offer();
    invalid.onion = "example.com".into();
    service.receive_offer(peer.public_key(), invalid);
    assert!(rx.try_recv().is_err());
    service.receive_offer(peer.public_key(), offer());
    assert!(rx.try_recv().is_ok());
    let (mut client, mut server) = tokio::io::duplex(4096);
    let id = job.track.file_id.clone();
    let task = tokio::spawn(async move {
        read_frame::<_, ClientFrame>(&mut server).await.unwrap();
        write_frame(
            &mut server,
            &ServerFrame::Welcome {
                version: 2,
                file_id: id.clone(),
                filename: "song.wav".into(),
                size: 48,
            },
        )
        .await
        .unwrap();
        read_frame::<_, ClientFrame>(&mut server).await.unwrap();
        write_frame(
            &mut server,
            &ServerFrame::FileData {
                size: 48,
                sha256: id,
            },
        )
        .await
        .unwrap();
        let mut corrupt = test_wav();
        corrupt[47] = 3;
        server.write_all(&corrupt).await.unwrap();
    });
    assert!(service
        .download_stream(&job, &offer(), &mut client)
        .await
        .unwrap_err()
        .contains("SHA-256"));
    task.await.unwrap();
    assert_eq!(fs::read_dir(&service.root).unwrap().count(), 0);
}

#[test]
fn rejects_unsafe_destinations_and_recovers_queued_downloads() {
    let (_dir, service, job) = fixture();
    for name in ["..", "../outside", "/tmp/outside", "a/b", "a\\b", ""] {
        assert!(destination_dir(&service.root, Some(name)).is_err());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            service.state_path.parent().unwrap(),
            service.root.join("Audiobooks"),
        )
        .unwrap();
        assert!(destination_dir(&service.root, Some("Book")).is_err());
    }
    let id = service
        .enqueue(
            job.track.clone(),
            vec![Keys::generate().public_key().to_hex()],
            None,
        )
        .unwrap();
    let same = service.enqueue(job.track, vec![], None).unwrap();
    assert_eq!(id, same);
    let reloaded = Downloads::new(
        service.client.clone(),
        service.tor.clone(),
        service.root.clone(),
        service.state_path.parent().unwrap(),
        service.library.clone(),
    )
    .unwrap();
    let restored = reloaded.transfers().unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].status, "Queued");
    assert_ne!(
        reloaded
            .jobs
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .request_id,
        id
    );
}

#[test]
fn remote_audiobook_metadata_is_bound_to_its_ordered_chapter_hashes() {
    let (_dir, _service, job) = fixture();
    let mut hash = Sha256::new();
    hash.update(b"napstr-audiobook-v1\0");
    hash.update(job.track.file_id.as_bytes());
    let id = hex::encode(hash.finalize());
    let keys = Keys::generate();
    let mut content = serde_json::json!({
        "protocol":"napstr/1", "audiobookId":id, "title":"A Book", "author":"Author", "narrator":"", "totalSize":48,
        "chapters":[{"position":1,"fileId":job.track.file_id,"filename":"Chapter.wav","title":"Chapter","format":"WAV","mime":"audio/wav","size":48}]
    });
    let event = EventBuilder::new(Kind::from(30423), content.to_string())
        .tags([Tag::identifier(&id)])
        .sign_with_keys(&keys)
        .unwrap();
    assert_eq!(remote_book(&event).unwrap().chapters.len(), 1);
    content["chapters"][0]["position"] = serde_json::json!(2);
    let event = EventBuilder::new(Kind::from(30423), content.to_string())
        .tags([Tag::identifier(&id)])
        .sign_with_keys(&keys)
        .unwrap();
    assert!(remote_book(&event).is_none());
}

#[test]
fn companion_requests_accept_all_known_seeders_and_select_three() {
    assert_eq!(
        seeder_candidates(vec![
            "d".into(),
            "b".into(),
            "a".into(),
            "a".into(),
            "c".into()
        ])
        .unwrap(),
        vec!["a", "b", "c"]
    );
    assert!(seeder_candidates(vec![]).is_err());
    assert_eq!(catalogue_query_token("The Song"), "song");
    assert_eq!(catalogue_query_token("the audio"), "napstr");
}
