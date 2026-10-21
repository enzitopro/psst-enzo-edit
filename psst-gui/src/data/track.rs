use std::{convert::TryFrom, sync::Arc, time::Duration};

use druid::{im::Vector, lens::Map, Data, Lens};
use itertools::Itertools;
use psst_core::item_id::{ItemId, ItemIdType};
use serde::{Deserialize, Serialize};

use crate::data::{
    config::{SortCriteria, SortOrder},
    AlbumLink, ArtistLink,
};

/// Deserializes the Web API's tracks, and what it serializes too, for keeping
/// the queue between sessions.
#[derive(Clone, Debug, Data, Lens, Deserialize, Serialize)]
pub struct Track {
    #[serde(default)]
    pub id: TrackId,
    pub name: Arc<str>,
    pub album: Option<AlbumLink>,
    pub artists: Vector<ArtistLink>,
    #[serde(rename = "duration_ms")]
    #[serde(deserialize_with = "super::utils::deserialize_millis")]
    #[serde(serialize_with = "super::utils::serialize_millis")]
    pub duration: Duration,
    pub disc_number: usize,
    pub track_number: usize,
    pub explicit: bool,
    pub is_local: bool,
    #[serde(default)]
    pub local_path: Option<Arc<str>>,
    pub is_playable: Option<bool>,
    #[serde(default)]
    pub popularity: Option<u32>,
    #[serde(skip)]
    pub track_pos: usize,
    pub lyrics: Option<Arc<[TrackLines]>>,
}

impl Track {
    pub fn lens_artist_name() -> impl Lens<Self, Arc<str>> {
        Map::new(
            |track: &Self| track.artist_name(),
            |_, _| {
                // Immutable.
            },
        )
    }

    pub fn lens_album_name() -> impl Lens<Self, Arc<str>> {
        Map::new(
            |track: &Self| track.album_name(),
            |_, _| {
                // Immutable.
            },
        )
    }

    pub fn artist_name(&self) -> Arc<str> {
        self.artists
            .front()
            .map(|artist| artist.name.clone())
            .unwrap_or_else(|| "Unknown".into())
    }

    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|artist| artist.name.clone())
            .join(", ")
    }

    pub fn album_name(&self) -> Arc<str> {
        self.album
            .as_ref()
            .map(|album| album.name.clone())
            .unwrap_or_else(|| "Unknown".into())
    }

    pub fn url(&self) -> String {
        format!("https://open.spotify.com/track/{}", self.id.0.to_base62())
    }

    /// A local playlist item that didn't match any file in the local folders.
    pub fn is_missing_local_file(&self) -> bool {
        self.is_local && self.local_path.is_none()
    }
}

/// Sorts tracks for display.  Text is compared ignoring case and accents, and
/// ties fall back to the next sensible field (e.g. album, then track number).
/// `DateAdded` is the order the tracks came in, kept in `track_pos`.
pub fn sort_tracks(
    tracks: &Vector<Arc<Track>>,
    criteria: SortCriteria,
    order: SortOrder,
) -> Vector<Arc<Track>> {
    use crate::webapi::local_library::normalize;

    let position = |t: &Track| (t.disc_number * 1000 + t.track_number) as u64;
    let mut keyed: Vec<((String, String, u64, usize), Arc<Track>)> = tracks
        .iter()
        .map(|track| {
            let key = match criteria {
                SortCriteria::Title => (
                    normalize(&track.name),
                    normalize(&track.artist_name()),
                    0,
                    track.track_pos,
                ),
                SortCriteria::Artist => (
                    normalize(&track.artist_name()),
                    normalize(&track.album_name()),
                    position(track),
                    track.track_pos,
                ),
                SortCriteria::Album => (
                    normalize(&track.album_name()),
                    normalize(&track.artist_name()),
                    position(track),
                    track.track_pos,
                ),
                SortCriteria::Duration => (
                    String::new(),
                    String::new(),
                    track.duration.as_millis() as u64,
                    track.track_pos,
                ),
                SortCriteria::DateAdded => (String::new(), String::new(), 0, track.track_pos),
            };
            (key, track.clone())
        })
        .collect();
    keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
    if order == SortOrder::Descending {
        keyed.reverse();
    }
    keyed.into_iter().map(|(_, track)| track).collect()
}

#[cfg(test)]
mod sort_tests {
    use super::*;
    use crate::webapi::local_library::{local_track, LocalItem};

    fn track(pos: usize, title: &str, artist: &str, album: &str, number: usize) -> Arc<Track> {
        let item = LocalItem {
            title: title.into(),
            artists: vec![artist.into()],
            album: album.into(),
            duration_ms: Some(1000 * pos as u64),
        };
        let mut track = local_track(&item, None, 1, number);
        track.track_pos = pos;
        Arc::new(track)
    }

    fn titles(tracks: &Vector<Arc<Track>>) -> Vec<&str> {
        tracks.iter().map(|t| t.name.as_ref()).collect()
    }

    #[test]
    fn sorts_ignoring_case_and_accents() {
        let tracks: Vector<_> = [
            track(0, "zebra", "B", "X", 1),
            track(1, "Álbum", "A", "X", 2),
            track(2, "beyoncé", "C", "X", 3),
            track(3, "Apple", "D", "X", 4),
        ]
        .into_iter()
        .collect();
        let sorted = sort_tracks(&tracks, SortCriteria::Title, SortOrder::Ascending);
        assert_eq!(titles(&sorted), vec!["Álbum", "Apple", "beyoncé", "zebra"]);
        let sorted = sort_tracks(&tracks, SortCriteria::Title, SortOrder::Descending);
        assert_eq!(titles(&sorted), vec!["zebra", "beyoncé", "Apple", "Álbum"]);
    }

    #[test]
    fn sorts_albums_by_track_number_and_restores_date_added() {
        let tracks: Vector<_> = [
            track(0, "Third", "Artist", "Album B", 3),
            track(1, "First", "Artist", "Album B", 1),
            track(2, "Other", "Artist", "album a", 7),
            track(3, "Second", "Artist", "Album B", 2),
        ]
        .into_iter()
        .collect();
        let by_album = sort_tracks(&tracks, SortCriteria::Album, SortOrder::Ascending);
        assert_eq!(titles(&by_album), vec!["Other", "First", "Second", "Third"]);
        let back = sort_tracks(&by_album, SortCriteria::DateAdded, SortOrder::Ascending);
        assert_eq!(titles(&back), vec!["Third", "First", "Other", "Second"]);
        let newest = sort_tracks(&by_album, SortCriteria::DateAdded, SortOrder::Descending);
        assert_eq!(titles(&newest), vec!["Second", "Other", "First", "Third"]);
    }
}

#[derive(Clone, Debug, Data, Lens, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackLines {
    pub start_time_ms: String,
    pub words: String,
    pub end_time_ms: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Hash, Deserialize, Serialize)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub struct TrackId(pub ItemId);

impl Data for TrackId {
    fn same(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl TryFrom<String> for TrackId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ItemId::from_base62(&value, ItemIdType::Track)
            .ok_or("Invalid ID")
            .map(Self)
    }
}

impl From<TrackId> for String {
    fn from(id: TrackId) -> Self {
        id.0.to_base62()
    }
}

#[derive(Clone, Data, Debug, Deserialize)]
#[allow(dead_code)]
pub struct AudioAnalysis {
    pub segments: Vector<AudioSegment>,
}

#[derive(Clone, Data, Debug, Deserialize)]
#[allow(dead_code)]
pub struct AudioSegment {
    #[serde(flatten)]
    pub interval: TimeInterval,
    pub loudness_start: f64,
    pub loudness_max: f64,
    pub loudness_max_time: f64,
}

#[derive(Clone, Data, Debug, Deserialize)]
#[allow(dead_code)]
pub struct TimeInterval {
    #[serde(deserialize_with = "super::utils::deserialize_secs")]
    pub start: Duration,
    #[serde(deserialize_with = "super::utils::deserialize_secs")]
    pub duration: Duration,
    pub confidence: f64,
}
