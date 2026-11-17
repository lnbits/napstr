use crate::audio;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};
use walkdir::WalkDir;

pub type Library = BTreeMap<String, SharedFile>;

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct SharedFile {
    pub protocol: &'static str,
    pub file_id: String,
    pub filename: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub format: String,
    pub mime: String,
    pub size: u64,
    pub license: &'static str,
    pub description: &'static str,
    pub tags: &'static str,
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(skip)]
    pub track_number: u32,
    #[serde(skip)]
    pub disc_number: u32,
}

#[derive(PartialEq, Eq, Clone)]
struct Stamp {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Stamp {
    fn new(metadata: &Metadata) -> Result<Self, String> {
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified().map_err(|e| e.to_string())?,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
}

#[derive(Default)]
pub struct Scanner {
    cache: HashMap<PathBuf, (Stamp, SharedFile)>,
}

// Reject symlinks in any component, including a directory replaced since the
// scan. Recheck at transfer time; the verified snapshot also binds the bytes.
pub fn checked_path(root: &Path, path: &Path) -> Result<(), String> {
    if !path.starts_with(root) || path.canonicalize().map_err(|e| e.to_string())? != path {
        return Err("shared path is outside the music folder or contains a symlink".into());
    }
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err("shared path is not a regular file".into());
    }
    Ok(())
}

impl Scanner {
    pub fn scan(&mut self, root: &Path) -> Result<Library, String> {
        if !root.is_dir() || root.canonicalize().map_err(|e| e.to_string())? != root {
            return Err("music directory disappeared or was replaced by a symlink".into());
        }
        let mut cache = HashMap::new();
        let mut library = Library::new();
        for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            let entry = entry.map_err(|e| format!("music scan failed: {e}"))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(extension.as_str(), "mp3" | "flac" | "wav" | "ogg" | "opus") {
                continue;
            }
            checked_path(root, path)?;
            let stamp = Stamp::new(&fs::metadata(path).map_err(|e| e.to_string())?)?;
            let file = if let Some((previous, file)) =
                self.cache.get(path).filter(|(s, _)| *s == stamp)
            {
                let _ = previous;
                file.clone()
            } else {
                let audio = match audio::validate_audio(path) {
                    Ok(audio) => audio,
                    Err(_) => continue,
                };
                let mut source = File::open(path).map_err(|e| e.to_string())?;
                let mut hash = Sha256::new();
                let mut buffer = vec![0; 256 * 1024];
                loop {
                    let count = source.read(&mut buffer).map_err(|e| e.to_string())?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
                if Stamp::new(&source.metadata().map_err(|e| e.to_string())?)? != stamp {
                    continue; // A file still being copied will be picked up next scan.
                }
                let filename = audio::sanitise_public_text(&entry.file_name().to_string_lossy());
                if filename.is_empty() {
                    continue;
                }
                SharedFile {
                    protocol: "napstr/1",
                    file_id: hex::encode(hash.finalize()),
                    filename,
                    title: audio.metadata.title,
                    artist: audio.metadata.artist,
                    album: audio.metadata.album,
                    format: audio.format.into(),
                    mime: audio.mime.into(),
                    size: stamp.size,
                    license: "unspecified",
                    description: "",
                    tags: "",
                    path: path.to_owned(),
                    track_number: audio.metadata.track_number,
                    disc_number: audio.metadata.disc_number,
                }
            };
            library
                .entry(file.file_id.clone())
                .or_insert_with(|| file.clone());
            cache.insert(path.to_owned(), (stamp, file));
            if cache.len() > 50_000 {
                return Err("music folder exceeds the daemon limit of 50000 audio files".into());
            }
        }
        self.cache = cache;
        Ok(library)
    }
}

#[cfg(test)]
pub fn test_wav() -> Vec<u8> {
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend(40u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(8000u32.to_le_bytes());
    wav.extend(16000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(4u32.to_le_bytes());
    wav.extend([0, 0, 0, 0]);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_audio_recursively_and_reconciles_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join("album")).unwrap();
        let song = root.join("album/song.wav");
        fs::write(&song, test_wav()).unwrap();
        fs::write(root.join("fake.mp3"), b"not audio").unwrap();
        fs::write(root.join("secret.txt"), b"private").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&song, root.join("link.wav")).unwrap();
        let mut scanner = Scanner::default();
        let first = scanner.scan(&root).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(scanner.cache.len(), 1);
        assert_eq!(
            scanner.scan(&root).unwrap().keys().collect::<Vec<_>>(),
            first.keys().collect::<Vec<_>>()
        );
        let json = serde_json::to_string(first.values().next().unwrap()).unwrap();
        assert!(!json.contains(root.to_str().unwrap()));
        let mut changed = test_wav();
        changed[47] = 1;
        fs::write(&song, changed).unwrap();
        let second = scanner.scan(&root).unwrap();
        assert_ne!(first.keys().next(), second.keys().next());
        fs::remove_file(song).unwrap();
        assert!(scanner.scan(&root).unwrap().is_empty());
    }
}
