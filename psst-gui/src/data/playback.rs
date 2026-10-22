use std::{
    fmt,
    fs::{self, File},
    io::{self, BufReader},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use druid::{im::Vector, Data, Lens};
use druid_enums::Matcher;
use psst_core::item_id::ItemId;
use serde::{Deserialize, Serialize};

use super::{AlbumLink, Config, Episode, Library, Nav, PlaylistLink, ShowLink, Track};

#[derive(Clone, Data, Lens)]
pub struct Playback {
    pub state: PlaybackState,
    pub now_playing: Option<NowPlaying>,
    pub queue_behavior: QueueBehavior,
    pub queue: Vector<QueueEntry>,
    /// What plays after the current item, in order, as the player tells.
    pub upcoming: Vector<QueueEntry>,
    /// What played before the current item this session, newest first.
    pub history: Vector<QueueEntry>,
    pub volume: f64,
}

impl Playback {
    /// Moves what played into `history` as `next` starts, unless it's the same
    /// item again (repeat one).
    pub fn remember_played(&mut self, next: &Playable) {
        const MAX_HISTORY: usize = 50;
        let Some(played) = &self.now_playing else {
            return;
        };
        if played.item.id() == next.id() {
            return;
        }
        self.history.push_front(QueueEntry {
            item: played.item.clone(),
            origin: played.origin.clone(),
        });
        // `truncate` past the end panics.
        if self.history.len() > MAX_HISTORY {
            self.history.truncate(MAX_HISTORY);
        }
    }
}

/// A change to what plays next, by position in `Playback::upcoming`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueueEdit {
    PlayNow(usize),
    MoveUp(usize),
    MoveDown(usize),
    Remove(usize),
}

impl QueueEdit {
    /// `upcoming` after the edit.  Playing an item now drops the ones before it.
    pub fn apply(self, mut upcoming: Vector<QueueEntry>) -> Vector<QueueEntry> {
        let len = upcoming.len();
        match self {
            QueueEdit::PlayNow(i) if i < len => upcoming.slice(i..),
            QueueEdit::MoveUp(i) if i > 0 && i < len => {
                upcoming.swap(i - 1, i);
                upcoming
            }
            QueueEdit::MoveDown(i) if i + 1 < len => {
                upcoming.swap(i, i + 1);
                upcoming
            }
            QueueEdit::Remove(i) if i < len => {
                upcoming.remove(i);
                upcoming
            }
            _ => upcoming,
        }
    }
}

#[derive(Clone, Debug, Data, Lens)]
pub struct QueueEntry {
    pub item: Playable,
    pub origin: PlaybackOrigin,
}

#[derive(Clone, Debug, Matcher)]
pub enum Playable {
    Track(Arc<Track>),
    Episode(Arc<Episode>),
}

impl Playable {
    pub fn track(&self) -> Option<&Arc<Track>> {
        if let Self::Track(track) = self {
            Some(track)
        } else {
            None
        }
    }

    pub fn id(&self) -> ItemId {
        match self {
            Playable::Track(track) => track.id.0,
            Playable::Episode(episode) => episode.id.0,
        }
    }

    pub fn name(&self) -> &Arc<str> {
        match self {
            Playable::Track(track) => &track.name,
            Playable::Episode(episode) => &episode.name,
        }
    }

    pub fn duration(&self) -> Duration {
        match self {
            Playable::Track(track) => track.duration,
            Playable::Episode(episode) => episode.duration,
        }
    }

    pub fn same(&self, other: &Self) -> bool {
        self.id() == other.id()
    }

    /// Local playlist items without a matching file can't be played.
    pub fn is_playable(&self) -> bool {
        match self {
            Playable::Track(track) => !track.is_missing_local_file(),
            Playable::Episode(_) => true,
        }
    }
}

impl Data for Playable {
    fn same(&self, other: &Self) -> bool {
        self.same(other)
    }
}

#[derive(Default, Copy, Clone, Debug, Data, Eq, PartialEq, Serialize, Deserialize)]
pub enum QueueBehavior {
    #[default]
    Sequential,
    Random,
    LoopTrack,
    LoopAll,
}

#[derive(Copy, Clone, Debug, Data, Eq, PartialEq)]
pub enum PlaybackState {
    Loading,
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Data, Lens)]
pub struct NowPlaying {
    pub item: Playable,
    pub origin: PlaybackOrigin,
    pub progress: Duration,

    // Although keeping a ref to the `Library` here is a bit of a hack, it dramatically
    // simplifies displaying the track context menu in the playback bar.
    pub library: Arc<Library>,
}

impl NowPlaying {
    pub fn cover_image_url(&self, width: f64, height: f64) -> Option<&str> {
        match &self.item {
            Playable::Track(track) => {
                let album = track.album.as_ref().or(match &self.origin {
                    PlaybackOrigin::Album(album) => Some(album),
                    _ => None,
                })?;
                Some(&album.image(width, height)?.url)
            }
            Playable::Episode(episode) => Some(&episode.image(width, height)?.url),
        }
    }

    pub fn cover_image_metadata(&self) -> Option<(&str, (u32, u32))> {
        match &self.item {
            Playable::Track(track) => track.album.as_ref().and_then(|album| {
                album.images.get(0).map(|img| {
                    (
                        &*img.url,
                        (
                            img.width.unwrap_or(0) as u32,
                            img.height.unwrap_or(0) as u32,
                        ),
                    )
                })
            }),
            Playable::Episode(episode) => episode.images.get(0).map(|img| {
                (
                    &*img.url,
                    (
                        img.width.unwrap_or(0) as u32,
                        img.height.unwrap_or(0) as u32,
                    ),
                )
            }),
        }
    }
}

#[derive(Clone, Debug, Data, PartialEq, Serialize, Deserialize)]
pub enum PlaybackOrigin {
    Home,
    Library,
    Album(AlbumLink),
    Playlist(PlaylistLink),
    Show(ShowLink),
    Search(Arc<str>),
}

impl PlaybackOrigin {
    pub fn to_nav(&self) -> Nav {
        match &self {
            PlaybackOrigin::Home => Nav::Home,
            PlaybackOrigin::Library => Nav::SavedTracks,
            PlaybackOrigin::Album(link) => Nav::AlbumDetail(link.clone(), None),
            PlaybackOrigin::Playlist(link) => Nav::PlaylistDetail(link.clone()),
            PlaybackOrigin::Show(link) => Nav::ShowDetail(link.clone()),
            PlaybackOrigin::Search(query) => Nav::SearchResults(query.clone()),
        }
    }
}

impl fmt::Display for PlaybackOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            PlaybackOrigin::Home => f.write_str("Home"),
            PlaybackOrigin::Library => f.write_str("Saved Tracks"),
            PlaybackOrigin::Album(link) => link.name.fmt(f),
            PlaybackOrigin::Playlist(link) => link.name.fmt(f),
            PlaybackOrigin::Show(link) => link.name.fmt(f),
            PlaybackOrigin::Search(query) => query.fmt(f),
        }
    }
}

/// The queue when Psst closed, to pick up from there.  Episodes aren't kept.
#[derive(Serialize, Deserialize)]
pub struct SavedPlayback {
    pub origin: PlaybackOrigin,
    pub tracks: Vec<Arc<Track>>,
    /// Of the playing track in `tracks`.
    pub position: usize,
    pub progress_ms: u64,
}

impl SavedPlayback {
    fn path() -> Option<PathBuf> {
        Config::config_dir().map(|dir| dir.join("playback.json"))
    }

    pub fn load() -> Option<Self> {
        let file = File::open(Self::path()?).ok()?;
        serde_json::from_reader(BufReader::new(file)).ok()
    }

    pub(super) fn of(playback: &Playback) -> Option<Self> {
        let now_playing = playback.now_playing.as_ref()?;
        let playing = now_playing.item.track()?;
        let tracks: Vec<_> = playback
            .queue
            .iter()
            .filter_map(|entry| entry.item.track().cloned())
            .collect();
        Some(Self {
            origin: now_playing.origin.clone(),
            position: tracks.iter().position(|track| track.id == playing.id)?,
            tracks,
            progress_ms: now_playing.progress.as_millis() as u64,
        })
    }

    /// Keeps what `playback` plays for the next session, or forgets the last
    /// one when nothing plays.
    pub fn save(playback: &Playback) {
        let Some(path) = Self::path() else {
            return;
        };
        let Some(saved) = Self::of(playback) else {
            let _ = fs::remove_file(path);
            return;
        };
        let written = serde_json::to_vec(&saved)
            .map_err(io::Error::from)
            .and_then(|json| super::utils::write_atomic(&path, &json));
        if let Err(err) = written {
            log::error!("couldn't save the playback queue: {err}");
        }
    }
}

#[derive(Clone, Debug, Data)]
pub struct PlaybackPayload {
    pub origin: PlaybackOrigin,
    pub items: Vector<Playable>,
    pub position: usize,
}
