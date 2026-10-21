use std::sync::Arc;

use druid::{im::Vector, Data, Lens};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::data::utils::sanitize_html_string;
use crate::data::{user::PublicUser, Image, Promise, Track, TrackId};

#[derive(Clone, Debug, Data, Lens)]
pub struct PlaylistDetail {
    pub playlist: Promise<Playlist, PlaylistLink>,
    pub tracks: Promise<PlaylistTracks, PlaylistLink>,
}

#[derive(Clone, Debug, Data, Lens, Deserialize)]
pub struct PlaylistAddTrack {
    pub link: PlaylistLink,
    pub track_id: TrackId,
}

#[derive(Clone, Debug, Data, Lens, Deserialize)]
pub struct PlaylistRemoveTrack {
    pub link: PlaylistLink,
    pub track_uri: Arc<str>,
}

/// Deserializes the Web API's playlists, and what it serializes too, for the
/// offline copy of the library.
#[derive(Clone, Debug, Data, Lens, Deserialize, Serialize)]
pub struct Playlist {
    pub id: Arc<str>,
    pub name: Arc<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vector<Image>>,
    #[serde(deserialize_with = "deserialize_description")]
    pub description: Arc<str>,
    // Spotify returns both a `tracks` object (with `total`) and, more recently,
    // a separate `items` key on playlist objects. Matching both (rename + alias)
    // makes serde error with "duplicate field `items`", so read only `tracks`.
    #[serde(rename = "tracks")]
    #[serde(deserialize_with = "deserialize_track_count")]
    #[serde(serialize_with = "serialize_track_count")]
    #[serde(default)]
    pub track_count: Option<usize>,
    pub owner: PublicUser,
    pub collaborative: bool,
    #[serde(rename = "public")]
    pub public: Option<bool>,
    /// Pinned to the top of the library in the official client.
    #[serde(default)]
    pub pinned: bool,
}

impl Playlist {
    pub fn link(&self) -> PlaylistLink {
        PlaylistLink {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    pub fn image(&self, width: f64, height: f64) -> Option<&Image> {
        self.images
            .as_ref()
            .and_then(|images| Image::at_least_of_size(images, width, height))
    }

    pub fn url(&self) -> String {
        format!("https://open.spotify.com/playlist/{id}", id = self.id)
    }
}

#[derive(Clone, Debug, Data, Lens)]
pub struct PlaylistTracks {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub tracks: Vector<Arc<Track>>,
}

impl PlaylistTracks {
    pub fn link(&self) -> PlaylistLink {
        PlaylistLink {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Clone, Debug, Data, Lens, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PlaylistLink {
    pub id: Arc<str>,
    pub name: Arc<str>,
}

/// As the Web API sends it, `{"total": 12}`.
fn serialize_track_count<S: Serializer>(
    count: &Option<usize>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct PlaylistTracksRef {
        total: Option<usize>,
    }
    PlaylistTracksRef { total: *count }.serialize(serializer)
}

fn deserialize_track_count<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct PlaylistTracksRef {
        total: Option<usize>,
    }

    // `tracks` can also be explicitly `null`.
    Ok(Option::<PlaylistTracksRef>::deserialize(deserializer)?.and_then(|tracks| tracks.total))
}

fn deserialize_description<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let description: String = String::deserialize(deserializer)?;
    Ok(sanitize_html_string(&description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_copy_round_trips() {
        let playlist: Playlist = serde_json::from_value(serde_json::json!({
            "id": "p",
            "name": "Rock & Roll",
            "images": [{ "url": "https://i.scdn.co/x", "width": 300, "height": 300 }],
            "description": "Songs <b>&amp;</b> more",
            "tracks": { "total": 12 },
            "owner": { "display_name": "Me", "id": "me" },
            "collaborative": false,
            "public": true
        }))
        .unwrap();
        let json = serde_json::to_string(&playlist).unwrap();
        let copy: Playlist = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&copy).unwrap(), json);
        assert_eq!(copy.track_count, Some(12));
        assert_eq!(copy.description, playlist.description);
        assert_eq!(&*copy.owner.display_name, "Me");
    }
}
