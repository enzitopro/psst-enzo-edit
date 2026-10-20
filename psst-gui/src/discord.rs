//! Discord Rich Presence: shows the playing track as "Listening to ..." on the
//! Discord profile, through the IPC pipe of the locally running Discord app.
//!
//! Runs on its own thread, so a missing or restarting Discord never blocks
//! playback.  The connection is retried in the background, and updates are
//! only sent when something visible changes (Discord rate-limits them).

use std::{
    sync::mpsc::{channel, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use discord_rich_presence::{
    activity::{Activity, ActivityType, Assets, Button, StatusDisplayType, Timestamps},
    DiscordIpc, DiscordIpcClient,
};

use crate::data::{Playable, Playback, PlaybackState};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);
const MIN_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
/// Seeks move the start timestamp; smaller jumps are just progress jitter.
const TIMESTAMP_TOLERANCE_SECS: i64 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Presence {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub track_url: Option<String>,
    /// Unix timestamps of the track start and end.
    pub timestamps: (i64, i64),
}

impl Presence {
    /// What to show for the current playback, `None` when nothing plays or it's paused.
    pub fn from_playback(playback: &Playback) -> Option<Self> {
        let now_playing = playback.now_playing.as_ref()?;
        let (title, artist, album, track_url) = match &now_playing.item {
            Playable::Track(track) => (
                track.name.to_string(),
                track.artist_names(),
                track.album.as_ref().map(|album| album.name.to_string()),
                (!track.is_local).then(|| track.url()),
            ),
            Playable::Episode(episode) => (
                episode.name.to_string(),
                episode.show.name.to_string(),
                None,
                None,
            ),
        };
        let timestamps = match playback.state {
            PlaybackState::Stopped | PlaybackState::Paused => return None,
            PlaybackState::Playing | PlaybackState::Loading => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let start = now - now_playing.progress.as_secs() as i64;
                let end = start + now_playing.item.duration().as_secs() as i64;
                (start, end)
            }
        };
        Some(Self {
            title,
            artist,
            album,
            cover_url: now_playing.cover_image_url(300.0, 300.0).map(String::from),
            track_url,
            timestamps,
        })
    }

    /// Equal apart from progress jitter.
    fn looks_like(&self, other: &Self) -> bool {
        (self.timestamps.0 - other.timestamps.0).abs() <= TIMESTAMP_TOLERANCE_SECS
            && self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.cover_url == other.cover_url
            && self.track_url == other.track_url
    }

    fn activity(&self) -> Activity<'_> {
        let mut activity = Activity::new()
            .activity_type(ActivityType::Listening)
            // "Listening to <artist>" in the member list, and on the profile card
            // instead of the Discord application's name.
            .name(fit(&self.artist))
            .status_display_type(StatusDisplayType::State)
            .details(fit(&self.title))
            .state(fit(&self.artist));
        let (start, end) = self.timestamps;
        let timestamps = Timestamps::new().start(start);
        activity = activity.timestamps(if end > start {
            timestamps.end(end)
        } else {
            timestamps
        });
        if let Some(cover_url) = &self.cover_url {
            let mut assets = Assets::new().large_image(cover_url.as_str());
            if let Some(album) = &self.album {
                assets = assets.large_text(fit(album));
            }
            activity = activity.assets(assets);
        }
        if let Some(track_url) = &self.track_url {
            activity = activity.buttons(vec![Button::new("Open in Spotify", track_url.as_str())]);
        }
        activity
    }
}

/// Discord wants 2 to 128 characters in its text fields.
fn fit(text: &str) -> String {
    let mut text: String = text.trim().chars().take(128).collect();
    while text.chars().count() < 2 {
        text.push('\u{2002}');
    }
    text
}

enum Message {
    /// The Discord application ID to connect with, `None` to turn it off.
    Configure(Option<String>),
    Update(Option<Presence>),
}

pub struct DiscordPresence {
    sender: Sender<Message>,
}

impl DiscordPresence {
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        thread::Builder::new()
            .name("discord-presence".into())
            .spawn(move || {
                let mut worker = Worker::default();
                loop {
                    match receiver.recv_timeout(RECONNECT_INTERVAL / 3) {
                        Ok(Message::Configure(app_id)) => worker.configure(app_id),
                        Ok(Message::Update(presence)) => worker.wanted = presence,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    worker.sync();
                }
                worker.disconnect();
            })
            .expect("failed to start the Discord presence thread");
        Self { sender }
    }

    pub fn configure(&self, app_id: Option<String>) {
        let _ = self.sender.send(Message::Configure(app_id));
    }

    pub fn update(&self, presence: Option<Presence>) {
        let _ = self.sender.send(Message::Update(presence));
    }
}

#[derive(Default)]
struct Worker {
    app_id: Option<String>,
    client: Option<DiscordIpcClient>,
    wanted: Option<Presence>,
    /// What Discord currently shows, as far as we know.
    shown: Option<Presence>,
    next_connect: Option<Instant>,
    last_update: Option<Instant>,
}

impl Worker {
    fn configure(&mut self, app_id: Option<String>) {
        let app_id = app_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        if app_id != self.app_id {
            self.disconnect();
            self.app_id = app_id;
            self.next_connect = None;
        }
    }

    fn disconnect(&mut self) {
        if let Some(mut client) = self.client.take() {
            let _ = client.clear_activity();
            let _ = client.close();
        }
        self.shown = None;
    }

    fn sync(&mut self) {
        let Some(app_id) = &self.app_id else {
            return;
        };
        let now = Instant::now();
        if self.client.is_none() {
            if self.next_connect.is_some_and(|at| now < at) {
                return;
            }
            let mut client = DiscordIpcClient::new(app_id);
            match client.connect() {
                Ok(()) => {
                    log::info!("connected to Discord");
                    self.client = Some(client);
                    self.shown = None;
                }
                Err(err) => {
                    // Discord isn't running, try again later.
                    log::debug!("cannot connect to Discord: {err}");
                    self.next_connect = Some(now + RECONNECT_INTERVAL);
                    return;
                }
            }
        }
        let up_to_date = match (&self.wanted, &self.shown) {
            (Some(wanted), Some(shown)) => wanted.looks_like(shown),
            (None, None) => true,
            _ => false,
        };
        if up_to_date
            || self
                .last_update
                .is_some_and(|at| now - at < MIN_UPDATE_INTERVAL)
        {
            return;
        }
        let client = self.client.as_mut().unwrap();
        let result = match &self.wanted {
            Some(presence) => client.set_activity(presence.activity()),
            None => client.clear_activity(),
        };
        self.last_update = Some(now);
        match result {
            Ok(()) => self.shown = self.wanted.clone(),
            Err(err) => {
                log::info!("lost connection to Discord: {err}");
                self.client = None;
                self.shown = None;
                self.next_connect = Some(now + Duration::from_secs(5));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(start: i64) -> Presence {
        Presence {
            title: "Marching".into(),
            artist: "Yeat".into(),
            album: Some("LYFESTYLE".into()),
            cover_url: Some("https://i.scdn.co/image/x".into()),
            track_url: None,
            timestamps: (start, start + 180),
        }
    }

    #[test]
    fn ignores_progress_jitter_but_not_seeks() {
        assert!(presence(1000).looks_like(&presence(1002)));
        assert!(!presence(1000).looks_like(&presence(1030)));
    }

    #[test]
    fn fits_discord_text_limits() {
        assert_eq!(fit("  Song  "), "Song");
        assert_eq!(fit("A").chars().count(), 2);
        assert_eq!(fit(&"x".repeat(300)).chars().count(), 128);
    }
}
