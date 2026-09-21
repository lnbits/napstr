//! Lets a paired phone with write access drive the desktop's own player.
//!
//! The queue belongs to the desktop's frontend, not to this service, so the
//! division of labour here is deliberate:
//!
//! * transport commands the audio player can carry out alone - pause, resume,
//!   seek, volume, stop - are applied directly to [`NativePlayer`];
//! * anything that needs to know what comes next - next, previous, repeat,
//!   shuffle, or playing at all when nothing is loaded - is forwarded to the
//!   frontend as a [`REMOTE_PLAYBACK_EVENT`]. Playing one particular track goes
//!   the same way and for the same reason: the window owns the queue, and it is
//!   the only side that knows whether this computer holds a track at all;
//! * the frontend reports its queue back through `publish_queue`, which is the
//!   only reason this module knows how long the queue is, and which is what
//!   lets a phone show a queue length and enable "next" for it.
//!
//! Nothing here decides what a phone may ask for. That is
//! `mobile::check_request_permission`, which refuses the whole request when the
//! pairing is read-only.

use crate::player::NativePlayer;
use chrono::Utc;
use napstr_remote_protocol::{PlaybackCommand, RemotePlaybackState, RemoteRepeat};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter};

/// Commands the frontend must carry out, because only it knows the queue.
pub const REMOTE_PLAYBACK_EVENT: &str = "napstr-remote-playback";
/// Fired on every command. Purely informational: it exists so the desktop can
/// show that a phone is driving it.
pub const REMOTE_ACTIVITY_EVENT: &str = "napstr-remote-activity";

/// How long the desktop keeps saying a phone is driving it after the last
/// command, so a single tap is still visible to whoever is at the computer.
const REMOTE_ACTIVITY_SECONDS: i64 = 25;

/// The desktop's own view of its queue, pushed by its frontend.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub len: usize,
    /// Index of the playing entry, or -1 when nothing is playing.
    pub index: i64,
    pub repeat: RemoteRepeat,
    pub shuffle: bool,
}

pub struct PlaybackBridge {
    player: Arc<NativePlayer>,
    app: AppHandle,
    queue: Mutex<QueueSnapshot>,
    /// Unix time until which a phone counts as having driven the desktop.
    remote_until: Mutex<i64>,
}

impl PlaybackBridge {
    pub fn new(player: Arc<NativePlayer>, app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            player,
            app,
            queue: Mutex::new(QueueSnapshot {
                index: -1,
                ..QueueSnapshot::default()
            }),
            remote_until: Mutex::new(0),
        })
    }

    /// The frontend owns the queue, so it is the one that says what is in it.
    pub fn publish_queue(&self, snapshot: QueueSnapshot) {
        if let Ok(mut queue) = self.queue.lock() {
            *queue = snapshot;
        }
    }

    /// What the desktop is doing, in the shape a phone can render.
    pub fn state(&self, db_path: &Path) -> RemotePlaybackState {
        let queue = self
            .queue
            .lock()
            .map(|queue| queue.clone())
            .unwrap_or_default();
        let status = match self.player.status() {
            Ok(status) => status,
            Err(error) => {
                return RemotePlaybackState {
                    error,
                    queue_index: -1,
                    updated_at: Utc::now().timestamp(),
                    ..RemotePlaybackState::default()
                }
            }
        };
        let (title, artist, album) =
            track_metadata(db_path, &status.file_id).unwrap_or_default();
        RemotePlaybackState {
            active: !status.file_id.is_empty(),
            playing: status.playing,
            file_id: status.file_id,
            title,
            artist,
            album,
            position_ms: seconds_to_ms(status.current_time),
            duration_ms: seconds_to_ms(status.duration),
            volume: f64::from(status.volume.clamp(0.0, 1.0)),
            queue_len: queue.len,
            queue_index: queue.index,
            repeat: queue.repeat,
            shuffle: queue.shuffle,
            remote_control: self.remote_control_active(),
            error: status.error,
            updated_at: Utc::now().timestamp(),
        }
    }

    /// Carry out one command from a phone, and report the result.
    pub fn apply(
        &self,
        db_path: &Path,
        command: PlaybackCommand,
    ) -> Result<RemotePlaybackState, String> {
        self.note_remote_activity();
        match command {
            PlaybackCommand::Play => {
                let status = self.player.status()?;
                if status.file_id.is_empty() {
                    // Nothing is loaded, so only the frontend knows what "play"
                    // means here: start whatever the computer was last on.
                    self.forward(&PlaybackCommand::Play)?;
                } else if !status.playing {
                    self.player.toggle()?;
                }
            }
            PlaybackCommand::Pause => {
                if self.player.status()?.playing {
                    self.player.toggle()?;
                }
            }
            PlaybackCommand::Toggle => {
                if self.player.file_id().is_some() {
                    self.player.toggle()?;
                } else {
                    self.forward(&PlaybackCommand::Play)?;
                }
            }
            PlaybackCommand::Stop => {
                self.player.stop_playback()?;
            }
            PlaybackCommand::Seek { position_ms } => {
                self.player.seek(position_ms as f64 / 1000.0)?;
            }
            PlaybackCommand::Volume { percent } => {
                self.player.set_volume(f32::from(percent.min(100)) / 100.0)?;
            }
            // The queue is the frontend's to move through, and it reports the
            // new position back through `publish_queue`.
            PlaybackCommand::Next
            | PlaybackCommand::Previous
            | PlaybackCommand::Repeat { .. }
            | PlaybackCommand::Shuffle { .. }
            | PlaybackCommand::PlayTrack { .. } => {
                self.forward(&command)?;
            }
        }
        Ok(self.state(db_path))
    }

    /// Hand a command to the desktop window.
    fn forward(&self, command: &PlaybackCommand) -> Result<(), String> {
        self.app
            .emit(REMOTE_PLAYBACK_EVENT, command)
            .map_err(|error| format!("could not reach the Napstr window: {error}"))
    }

    fn note_remote_activity(&self) {
        let now = Utc::now().timestamp();
        if let Ok(mut until) = self.remote_until.lock() {
            *until = now + REMOTE_ACTIVITY_SECONDS;
        }
        let _ = self.app.emit(REMOTE_ACTIVITY_EVENT, now);
    }

    fn remote_control_active(&self) -> bool {
        self.remote_until
            .lock()
            .map(|until| *until > Utc::now().timestamp())
            .unwrap_or(false)
    }
}

fn seconds_to_ms(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds < 0.0 {
        return 0;
    }
    (seconds * 1000.0).round() as u64
}

/// Title, artist and album of a loaded track. Covers the desktop's own library
/// only: a track the desktop is playing from a relay is not in `files`, and the
/// phone then shows the file id on its own.
fn track_metadata(db_path: &Path, file_id: &str) -> Option<(String, String, String)> {
    if file_id.is_empty() {
        return None;
    }
    let connection = crate::open_connection(db_path).ok()?;
    crate::load_files_by_id(&connection, &[file_id.to_string()])
        .ok()?
        .into_iter()
        .next()
        .map(|file| (file.title, file.artist, file.album))
}
