use crate::{
    library::{Library, SharedFile},
    server::{now, Offer, Server},
};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::RwLock,
    time::{sleep, timeout},
};

const PACE: Duration = Duration::from_millis(75);

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
enum Signal {
    #[serde(rename = "DOWNLOAD_REQUEST")]
    Request {
        protocol: String,
        request_id: String,
        file_id: String,
    },
    #[serde(rename = "DOWNLOAD_OFFER")]
    Offer { protocol: String, offer: Offer },
    #[serde(rename = "DOWNLOAD_REFUSED")]
    Refused {
        protocol: String,
        request_id: String,
        file_id: String,
        reason: String,
    },
}

fn signal_tags() -> Vec<Tag> {
    vec![
        Tag::expiration(Timestamp::from(now() + 20 * 60)),
        Tag::client("Napstr"),
    ]
}

pub async fn send(client: &Client, builder: EventBuilder) -> Result<(), String> {
    let output = timeout(Duration::from_secs(20), client.send_event_builder(builder))
        .await
        .map_err(|_| "relay publication timed out")?
        .map_err(|e| e.to_string())?;
    if output.success.is_empty() {
        return Err("no relay accepted the event".into());
    }
    Ok(())
}

pub async fn connect(client: &Client, pubkey: PublicKey, relays: &[String]) -> Result<(), String> {
    client.automatic_authentication(true);
    for relay in relays {
        client.add_relay(relay).await.map_err(|e| e.to_string())?;
    }
    client.connect().await;
    client
        .subscribe(
            Filter::new().kind(Kind::GiftWrap).pubkey(pubkey).limit(0),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    let tags = relays
        .iter()
        .map(|r| Tag::parse(["relay", r]))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    send(client, EventBuilder::new(Kind::from(10050), "").tags(tags)).await
}

fn valid_gift(gift: &UnwrappedGift, receiver: PublicKey) -> bool {
    let rumor = &gift.rumor;
    if rumor.kind != Kind::PrivateDirectMessage
        || rumor.pubkey != gift.sender
        || !rumor.tags.public_keys().any(|p| *p == receiver)
        || !rumor
            .tags
            .expiration()
            .is_some_and(|expiry| expiry.as_secs() > now())
        || rumor.created_at.as_secs() > now() + 60
        || rumor.created_at.as_secs() + 20 * 60 < now()
        || rumor.content.len() > 64 * 1024
    {
        return false;
    }
    true
}

fn request(gift: UnwrappedGift, receiver: PublicKey) -> Option<(PublicKey, String, String)> {
    if !valid_gift(&gift, receiver) {
        return None;
    }
    let rumor = gift.rumor;
    match serde_json::from_str::<Signal>(&rumor.content).ok()? {
        Signal::Request {
            protocol,
            request_id,
            file_id,
        } if protocol == "napstr/1"
            && uuid::Uuid::parse_str(&request_id).is_ok()
            && file_id.len() == 64
            && file_id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) =>
        {
            Some((gift.sender, request_id, file_id))
        }
        _ => None,
    }
}

pub async fn inbox(
    client: Client,
    receiver: PublicKey,
    server: Arc<Server>,
    onion: String,
    downloads: Arc<crate::downloads::Downloads>,
) -> Result<(), String> {
    let mut notifications = client.notifications();
    let mut seen = HashMap::<(PublicKey, String), u64>::new();
    loop {
        let notification = match notifications.recv().await {
            Ok(value) => value,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return Err("Nostr notification stream closed".into()),
        };
        let RelayPoolNotification::Event { event, .. } = notification else {
            continue;
        };
        if event.kind != Kind::GiftWrap || event.verify().is_err() {
            continue;
        }
        let Ok(gift) = client.unwrap_gift_wrap(&event).await else {
            continue;
        };
        if !valid_gift(&gift, receiver) {
            continue;
        }
        match serde_json::from_str::<Signal>(&gift.rumor.content) {
            Ok(Signal::Offer { protocol, offer }) if protocol == "napstr/1" => {
                downloads.receive_offer(gift.sender, offer);
                continue;
            }
            Ok(Signal::Refused {
                protocol,
                request_id,
                file_id,
                ..
            }) if protocol == "napstr/1" => {
                downloads.receive_refusal(gift.sender, &request_id, &file_id);
                continue;
            }
            _ => {}
        }
        let Some((sender, request_id, file_id)) = request(gift, receiver) else {
            continue;
        };
        seen.retain(|_, expires| *expires > now());
        let key = (sender, request_id.clone());
        if seen.contains_key(&key) || seen.len() >= 4096 {
            continue;
        }
        seen.insert(key, now() + 20 * 60);
        let response = match server
            .offer(request_id.clone(), file_id.clone(), sender.to_hex(), &onion)
            .await
        {
            Ok(offer) => Signal::Offer {
                protocol: "napstr/1".into(),
                offer,
            },
            Err(_) => Signal::Refused {
                protocol: "napstr/1".into(),
                request_id,
                file_id,
                reason: "file unavailable or seeder busy; try again later".into(),
            },
        };
        let content = serde_json::to_string(&response).map_err(|e| e.to_string())?;
        let delivered = matches!(timeout(Duration::from_secs(20), client.send_private_msg(sender, content, signal_tags())).await, Ok(Ok(output)) if !output.success.is_empty());
        if !delivered {
            if let Signal::Offer { offer, .. } = response {
                server.revoke(&offer.capability).await;
            }
            eprintln!("Could not deliver private response; waiting for another request");
        }
    }
}

fn search_tokens(file: &SharedFile) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "audio", "be", "by", "flac", "for", "from", "in",
        "is", "it", "mp3", "of", "ogg", "on", "opus", "or", "the", "to", "wav", "with", "napstr",
    ];
    let fields = [&file.filename, &file.title, &file.artist, &file.album];
    let tokens: Vec<Vec<_>> = fields
        .iter()
        .map(|field| {
            field
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(str::to_lowercase)
                .filter(|s| s.chars().count() <= 32 && !STOP.contains(&s.as_str()))
                .collect()
        })
        .collect();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for index in 0..tokens.iter().map(Vec::len).max().unwrap_or(0) {
        for field in &tokens {
            if let Some(token) = field.get(index) {
                if seen.insert(token.clone()) {
                    result.push(token.clone());
                }
                if result.len() == 20 {
                    return result;
                }
            }
        }
    }
    result
}

fn catalogue(file: &SharedFile) -> Result<(EventBuilder, String), String> {
    let content = serde_json::to_string(file).map_err(|e| e.to_string())?;
    let fingerprint = hex::encode(Sha256::digest(content.as_bytes()));
    let size = file.size.to_string();
    let mut tags = [
        ["d", file.file_id.as_str()],
        ["t", "napstr"],
        ["x", file.file_id.as_str()],
        ["name", file.filename.as_str()],
        ["size", &size],
        ["m", file.mime.as_str()],
        ["alt", "Napstr shared file catalogue entry"],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| e.to_string())?;
    for token in search_tokens(file) {
        tags.push(Tag::parse(["t", &token]).map_err(|e| e.to_string())?);
    }
    Ok((
        EventBuilder::new(Kind::from(30421), content).tags(tags),
        fingerprint,
    ))
}

async fn reconcile(
    client: &Client,
    library: &Library,
    published: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let removed: Vec<_> = published
        .keys()
        .filter(|id| !library.contains_key(*id))
        .cloned()
        .collect();
    for id in removed {
        send(
            client,
            EventBuilder::new(
                Kind::from(30421),
                r#"{"protocol":"napstr/1","deleted":true}"#,
            )
            .tags([Tag::identifier(&id), Tag::hashtag("napstr")]),
        )
        .await?;
        published.remove(&id);
        sleep(PACE).await;
    }
    for (id, file) in library {
        let (event, fingerprint) = catalogue(file)?;
        if published.get(id) == Some(&fingerprint) {
            continue;
        }
        send(client, event).await?;
        published.insert(id.clone(), fingerprint);
        sleep(PACE).await;
    }
    Ok(())
}

pub async fn publisher(
    client: Client,
    library: Arc<RwLock<Library>>,
    state_path: PathBuf,
) -> Result<(), String> {
    let mut published: BTreeMap<String, String> = match std::fs::read(&state_path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "invalid daemon publication state")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.to_string()),
    };
    // Republish once per process, including when the relay list has changed.
    for fingerprint in published.values_mut() {
        fingerprint.clear();
    }
    loop {
        let snapshot = library.read().await.clone();
        let result = reconcile(&client, &snapshot, &mut published).await;
        let parent = state_path.parent().ok_or("state file has no parent")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temp, &published).map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(&state_path).map_err(|e| e.to_string())?;
        if let Err(error) = result {
            eprintln!("Catalogue publication will retry: {error}");
        }
        sleep(Duration::from_secs(60)).await;
    }
}

pub async fn availability(
    client: &Client,
    ids: &[String],
    previous_batches: usize,
) -> Result<usize, String> {
    let batches = ids.len().div_ceil(400).max(1);
    for index in 0..batches.max(previous_batches) {
        let batch = ids
            .get(index * 400..(index * 400 + 400).min(ids.len()))
            .unwrap_or(&[]);
        send(
            client,
            EventBuilder::new(
                Kind::from(30422),
                serde_json::to_string(batch).map_err(|e| e.to_string())?,
            )
            .tags([
                Tag::identifier(format!("availability-{index:04}")),
                Tag::hashtag("napstr-availability"),
                Tag::expiration(Timestamp::from(now() + 10 * 60)),
            ]),
        )
        .await?;
        sleep(PACE).await;
    }
    Ok(batches)
}

pub async fn heartbeat(client: Client, library: Arc<RwLock<Library>>) -> Result<(), String> {
    let mut batches = 1;
    loop {
        let ids: Vec<_> = library.read().await.keys().cloned().collect();
        match availability(&client, &ids, batches).await {
            Ok(count) => batches = count,
            Err(error) => eprintln!("Availability publication will retry: {error}"),
        }
        sleep(Duration::from_secs(240)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_encrypted_desktop_requests_and_rejects_expired_signals() {
        let sender = Keys::generate();
        let receiver = Keys::generate();
        let request_id = uuid::Uuid::new_v4().to_string();
        let content = serde_json::to_string(&Signal::Request {
            protocol: "napstr/1".into(),
            request_id: request_id.clone(),
            file_id: "a".repeat(64),
        })
        .unwrap();
        let client = Client::new(receiver.clone());
        let gift =
            EventBuilder::private_msg(&sender, receiver.public_key(), &content, signal_tags())
                .await
                .unwrap();
        assert!(!gift.content.contains(&request_id));
        let decoded = request(
            client.unwrap_gift_wrap(&gift).await.unwrap(),
            receiver.public_key(),
        )
        .unwrap();
        assert_eq!(decoded, (sender.public_key(), request_id, "a".repeat(64)));
        let expired = EventBuilder::private_msg(
            &sender,
            receiver.public_key(),
            &content,
            [Tag::expiration(Timestamp::from(now() - 1))],
        )
        .await
        .unwrap();
        assert!(request(
            client.unwrap_gift_wrap(&expired).await.unwrap(),
            receiver.public_key()
        )
        .is_none());
        let untagged = EventBuilder::private_msg(&sender, receiver.public_key(), &content, [])
            .await
            .unwrap();
        assert!(request(
            client.unwrap_gift_wrap(&untagged).await.unwrap(),
            receiver.public_key()
        )
        .is_none());
        assert!(Client::new(Keys::generate())
            .unwrap_gift_wrap(&gift)
            .await
            .is_err());
    }

    #[test]
    fn catalogue_and_offers_match_desktop_wire_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("An Artist - Song.wav"),
            crate::library::test_wav(),
        )
        .unwrap();
        let library = crate::library::Scanner::default()
            .scan(&dir.path().canonicalize().unwrap())
            .unwrap();
        let file = library.values().next().unwrap();
        let event = catalogue(file)
            .unwrap()
            .0
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert_eq!(event.kind, Kind::from(30421));
        assert_eq!(event.tags.identifier(), Some(file.file_id.as_str()));
        assert!(event.tags.hashtags().any(|t| t == "artist"));
        assert!(!event.content.contains(dir.path().to_str().unwrap()));
        let offer = Signal::Offer {
            protocol: "napstr/1".into(),
            offer: Offer {
                request_id: "request".into(),
                file_id: file.file_id.clone(),
                onion: "test.onion".into(),
                port: 80,
                capability: "secret".into(),
                expires_at: now() + 900,
            },
        };
        let json = serde_json::to_value(offer).unwrap();
        assert_eq!(json["type"], "DOWNLOAD_OFFER");
        assert_eq!(json["offer"]["requestId"], "request");
        assert!(json["offer"].get("expiresAt").is_some());
    }
}
