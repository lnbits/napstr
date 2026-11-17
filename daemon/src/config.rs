use nostr_sdk::prelude::*;
use std::{env, path::PathBuf, time::Duration};

pub struct Config {
    pub keys: Keys,
    pub music_dir: PathBuf,
    pub data_dir: PathBuf,
    pub relays: Vec<String>,
    pub scan_interval: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::load(|name| env::var(name).ok())
    }

    fn load(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let secret = get("NAPSTR_NSEC").ok_or("NAPSTR_NSEC is required")?;
        // Only accept nsec, and never include the supplied secret in errors.
        let secret = SecretKey::from_bech32(secret.trim())
            .map_err(|_| "NAPSTR_NSEC must be a valid nsec private key")?;
        let music_dir = get("NAPSTR_MUSIC_DIR").ok_or("NAPSTR_MUSIC_DIR is required")?;
        let music_dir = PathBuf::from(music_dir);
        if !music_dir.is_absolute() || !music_dir.is_dir() {
            return Err("NAPSTR_MUSIC_DIR must be an existing absolute directory".into());
        }
        let music_dir = music_dir.canonicalize().map_err(|e| e.to_string())?;
        let data_dir =
            PathBuf::from(get("NAPSTR_DATA_DIR").unwrap_or_else(|| "/var/lib/napstrd".into()));
        if !data_dir.is_absolute() {
            return Err("NAPSTR_DATA_DIR must be an absolute directory".into());
        }
        let relays = get("NAPSTR_RELAYS")
            .unwrap_or_else(|| "wss://relay.damus.io,wss://nos.lol,wss://relay.nostr.com,wss://relay.primal.net,wss://relay.snort.social,wss://nostr.mom,wss://relay.nostr.band".into())
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                if !value.starts_with("wss://") || RelayUrl::parse(value).is_err() {
                    return Err("NAPSTR_RELAYS must contain comma-separated wss:// relay URLs");
                }
                Ok(value.to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if relays.is_empty() {
            return Err("NAPSTR_RELAYS must contain at least one relay".into());
        }
        let seconds = get("NAPSTR_SCAN_INTERVAL")
            .unwrap_or_else(|| "60".into())
            .parse::<u64>()
            .map_err(|_| "NAPSTR_SCAN_INTERVAL must be an integer from 10 to 86400 seconds")?;
        if !(10..=86400).contains(&seconds) {
            return Err("NAPSTR_SCAN_INTERVAL must be from 10 to 86400 seconds".into());
        }
        Ok(Self {
            keys: Keys::new(secret),
            music_dir,
            data_dir,
            relays,
            scan_interval: Duration::from_secs(seconds),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_configuration_without_disclosing_secrets() {
        let folder = tempfile::tempdir().unwrap();
        let secret = Keys::generate().secret_key().to_bech32().unwrap();
        let get = |name: &str| match name {
            "NAPSTR_NSEC" => Some(secret.clone()),
            "NAPSTR_MUSIC_DIR" => Some(folder.path().display().to_string()),
            _ => None,
        };
        let config = Config::load(get).unwrap();
        assert_eq!(config.scan_interval.as_secs(), 60);
        assert_eq!(config.music_dir, folder.path().canonicalize().unwrap());
        assert!(Config::load(|name| match name {
            "NAPSTR_NSEC" => Some("private-invalid-secret".into()),
            _ => get(name),
        })
        .err()
        .unwrap()
        .contains("valid nsec"));
        for (name, value) in [
            ("NAPSTR_RELAYS", ""),
            ("NAPSTR_RELAYS", "https://example.com"),
            ("NAPSTR_SCAN_INTERVAL", "0"),
            ("NAPSTR_MUSIC_DIR", "relative"),
        ] {
            assert!(Config::load(|key| if key == name {
                Some(value.into())
            } else {
                get(key)
            })
            .is_err());
        }
    }
}
