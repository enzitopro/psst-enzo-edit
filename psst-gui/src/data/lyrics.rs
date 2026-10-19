use std::sync::Arc;

use druid::{im::Vector, Data, Lens};
use serde::{Deserialize, Deserializer, Serialize};

use crate::data::{utils::deserialize_null_arc_str, Promise, TrackLines};

/// Lyrics of the playing track, from Spotify or LRCLIB.
#[derive(Clone, Debug, Data, Lens)]
pub struct Lyrics {
    pub lines: Vector<TrackLines>,
    pub source: Arc<str>,
    /// Whether the lines carry timestamps, i.e. can follow the playback.
    pub synced: bool,
}

impl Lyrics {
    /// Fills in the end time of every line with the start of the next one,
    /// Spotify mostly leaves it at zero and LRC files don't have it at all.
    pub fn new(lines: impl IntoIterator<Item = TrackLines>, source: &str) -> Self {
        let mut lines: Vector<TrackLines> = lines.into_iter().collect();
        let synced = lines.iter().any(|line| line.start_ms() > 0);
        if synced {
            let starts: Vec<u64> = lines.iter().map(TrackLines::start_ms).collect();
            for (i, line) in lines.iter_mut().enumerate() {
                let end = starts.get(i + 1).copied().unwrap_or(u64::MAX);
                line.end_time_ms = end.to_string();
            }
        }
        Self {
            lines,
            source: source.into(),
            synced,
        }
    }
}

impl TrackLines {
    pub fn start_ms(&self) -> u64 {
        self.start_time_ms.parse().unwrap_or(0)
    }

    pub fn end_ms(&self) -> u64 {
        self.end_time_ms.parse().unwrap_or(0)
    }
}

/// A lyrics record from LRCLIB.
#[derive(Clone, Debug, Data, Lens, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsCandidate {
    pub id: i64,
    #[serde(default, deserialize_with = "deserialize_null_arc_str")]
    pub track_name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_null_arc_str")]
    pub artist_name: Arc<str>,
    #[serde(default, deserialize_with = "deserialize_null_arc_str")]
    pub album_name: Arc<str>,
    /// In seconds.  Some records have it as `null`.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub duration: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<Arc<str>>,
    #[serde(default)]
    pub synced_lyrics: Option<Arc<str>>,
}

impl LyricsCandidate {
    pub fn has_lyrics(&self) -> bool {
        let non_empty = |s: &Option<Arc<str>>| s.as_deref().is_some_and(|s| !s.trim().is_empty());
        self.instrumental || non_empty(&self.synced_lyrics) || non_empty(&self.plain_lyrics)
    }

    pub fn is_synced(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Manual lyrics search in the lyrics view.
#[derive(Clone, Debug, Data, Lens)]
pub struct LyricsSearch {
    pub query: String,
    /// Deferred by the query, errors as text.
    pub results: Promise<Vector<LyricsCandidate>, String, String>,
}

impl Default for LyricsSearch {
    fn default() -> Self {
        Self {
            query: String::new(),
            results: Promise::Empty,
        }
    }
}
