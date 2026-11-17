// Included by downloads.rs: validate the same public audiobook wire format as Napstr.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BookEntry {
    protocol: String,
    audiobook_id: String,
    title: String,
    author: String,
    narrator: String,
    total_size: u64,
    chapters: Vec<BookChapter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BookChapter {
    position: usize,
    file_id: String,
    filename: String,
    title: String,
    format: String,
    mime: String,
    size: u64,
}

fn remote_book(event: &Event) -> Option<RemoteAudiobook> {
    if event.kind != Kind::from(30423)
        || event.content.len() > 128 * 1024
        || event.verify().is_err()
    {
        return None;
    }
    let entry: BookEntry = serde_json::from_str(&event.content).ok()?;
    if entry.protocol != "napstr/1"
        || entry.title.is_empty()
        || event.tags.identifier() != Some(entry.audiobook_id.as_str())
        || entry.chapters.is_empty()
        || entry.chapters.len() > 500
    {
        return None;
    }
    if [&entry.title, &entry.author, &entry.narrator]
        .iter()
        .any(|s| s.chars().count() > 256 || s.chars().any(char::is_control))
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"napstr-audiobook-v1\0");
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for (index, chapter) in entry.chapters.iter().enumerate() {
        if chapter.position != index + 1
            || !valid_id(&chapter.file_id)
            || !seen.insert(&chapter.file_id)
            || chapter.size == 0
            || !valid_audio_claim(&chapter.filename, &chapter.format, &chapter.mime)
            || [&chapter.filename, &chapter.title]
                .iter()
                .any(|s| s.chars().count() > 256 || s.chars().any(char::is_control))
        {
            return None;
        }
        total = total.checked_add(chapter.size)?;
        hash.update(chapter.file_id.as_bytes());
    }
    if total != entry.total_size || hex::encode(hash.finalize()) != entry.audiobook_id {
        return None;
    }
    let source = RemoteSource {
        pubkey: event.pubkey.to_hex(),
        display_name: event.pubkey.to_hex()[..12].into(),
    };
    Some(RemoteAudiobook {
        audiobook_id: entry.audiobook_id,
        title: entry.title.clone(),
        author: entry.author.clone(),
        narrator: entry.narrator,
        total_size: total,
        chapters: entry
            .chapters
            .into_iter()
            .map(|chapter| RemoteTrack {
                file_id: chapter.file_id,
                filename: chapter.filename,
                title: chapter.title,
                artist: entry.author.clone(),
                album: entry.title.clone(),
                format: chapter.format,
                mime: chapter.mime,
                size: chapter.size,
                tags: "audiobook".into(),
                local: false,
                sources: vec![source.clone()],
            })
            .collect(),
    })
}
