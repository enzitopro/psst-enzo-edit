//! Responses of the `api-partner.spotify.com` pathfinder (GraphQL) operations
//! used for the library list and for playlists the public Web API won't serve,
//! like the Daily Mixes and other Spotify-generated mixes.
//!
//! The persisted-query hashes come from the web player bundle and change from
//! time to time.  Callers fall back to the public Web API when these fail.

use std::{sync::Arc, time::Duration};

use druid::im::Vector;
use psst_core::item_id::ItemId;
use serde::Deserialize;
use serde_json::Value;

use time::Date;

use super::local_library::{LocalItem, LocalLibrary};
use crate::data::{
    utils::sanitize_html_string, Album, AlbumLink, AlbumType, Artist, ArtistLink, Copyright,
    CopyrightType, DatePrecision, Image, Playlist, PublicUser, Track, TrackId,
};

pub const LIBRARY_V3: (&str, &str) = (
    "libraryV3",
    "390c78e5b951029bad359785e69b07b536a509c581cbcd0aded5e5067f187455",
);
pub const FETCH_PLAYLIST: (&str, &str) = (
    "fetchPlaylist",
    "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf",
);
pub const GET_ALBUM: (&str, &str) = (
    "getAlbum",
    "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f",
);

fn id_from_uri(uri: &str) -> Arc<str> {
    uri.rsplit(':').next().unwrap_or_default().into()
}

#[derive(Deserialize)]
struct Sources {
    #[serde(default)]
    sources: Vec<Source>,
}

#[derive(Deserialize)]
struct Source {
    url: String,
    #[serde(default)]
    width: Option<usize>,
    #[serde(default)]
    height: Option<usize>,
}

impl Sources {
    fn images(&self) -> Vector<Image> {
        self.sources
            .iter()
            .map(|source| Image {
                url: source.url.as_str().into(),
                width: source.width,
                height: source.height,
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct ImageList {
    #[serde(default)]
    items: Vec<Sources>,
}

#[derive(Deserialize)]
struct Owner {
    data: OwnerData,
}

#[derive(Deserialize)]
struct OwnerData {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(default)]
    can_edit_items: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderPlaylist {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    images: Option<ImageList>,
    #[serde(default)]
    owner_v2: Option<Owner>,
    #[serde(default)]
    current_user_capabilities: Option<Capabilities>,
    #[serde(default)]
    content: Option<Content>,
}

impl PathfinderPlaylist {
    fn to_playlist(&self, pinned: bool) -> Option<Playlist> {
        if self.typename != "Playlist" {
            return None;
        }
        let uri = self.uri.as_deref()?;
        let owner = self.owner_v2.as_ref().map(|owner| &owner.data);
        Some(Playlist {
            id: id_from_uri(uri),
            name: self.name.as_deref().unwrap_or_default().into(),
            images: Some(
                self.images
                    .as_ref()
                    .and_then(|images| images.items.first())
                    .map(Sources::images)
                    .unwrap_or_default(),
            ),
            description: sanitize_html_string(self.description.as_deref().unwrap_or_default()),
            track_count: self.content.as_ref().map(|content| content.total_count),
            owner: PublicUser {
                display_name: owner
                    .and_then(|o| o.name.as_deref())
                    .unwrap_or_default()
                    .into(),
                id: owner
                    .and_then(|o| o.username.as_deref())
                    .unwrap_or_default()
                    .into(),
            },
            // The library only tells us whether we can add items, which is
            // what `collaborative` is used for there.
            collaborative: self
                .current_user_capabilities
                .as_ref()
                .is_some_and(|caps| caps.can_edit_items),
            public: None,
            pinned,
        })
    }
}

#[derive(Deserialize)]
pub struct LibraryResponse {
    data: LibraryData,
}

#[derive(Deserialize)]
struct LibraryData {
    me: LibraryMe,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryMe {
    library_v3: LibraryPage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryPage {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    items: Vec<LibraryItem>,
    #[serde(default)]
    total_count: usize,
}

#[derive(Deserialize)]
struct LibraryItem {
    #[serde(default)]
    pinned: bool,
    item: Wrapper,
}

#[derive(Deserialize)]
struct Wrapper {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    data: Option<Value>,
}

impl LibraryResponse {
    pub fn total_count(&self) -> usize {
        self.data.me.library_v3.total_count
    }

    pub fn item_count(&self) -> usize {
        self.data.me.library_v3.items.len()
    }

    /// The data of this page's items of type `typename` and whether each one is
    /// pinned, in library order.
    fn items_of(self, typename: &str) -> Result<Vec<(bool, Value)>, String> {
        let page = self.data.me.library_v3;
        if page.typename != "LibraryPage" {
            return Err(format!("unexpected library response: {}", page.typename));
        }
        Ok(page
            .items
            .into_iter()
            .filter(|item| item.item.typename == typename)
            .filter_map(|item| Some((item.pinned, item.item.data?)))
            .collect())
    }

    /// Playlists of this page, in library order.  Folders, liked songs and
    /// anything that isn't a playlist are skipped (folders are flattened by
    /// the request).
    pub fn into_playlists(self) -> Result<Vec<Playlist>, String> {
        Ok(self
            .items_of("PlaylistResponseWrapper")?
            .into_iter()
            .filter_map(|(pinned, data)| {
                let playlist: PathfinderPlaylist = serde_json::from_value(data).ok()?;
                playlist.to_playlist(pinned)
            })
            .collect())
    }

    /// Followed artists of this page, in library order.
    pub fn into_artists(self) -> Result<Vec<Artist>, String> {
        Ok(self
            .items_of("ArtistResponseWrapper")?
            .into_iter()
            .filter_map(|(_, data)| {
                let artist: PathfinderArtist = serde_json::from_value(data).ok()?;
                Some(Artist {
                    id: id_from_uri(&artist.uri),
                    name: artist.profile.name.into(),
                    images: artist
                        .visuals
                        .and_then(|visuals| visuals.avatar_image)
                        .map(|image| image.images())
                        .unwrap_or_default(),
                })
            })
            .collect())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtistVisuals {
    #[serde(default)]
    avatar_image: Option<Sources>,
}

#[derive(Deserialize)]
pub struct PlaylistResponse {
    data: PlaylistData,
}

#[derive(Deserialize)]
struct PlaylistData {
    #[serde(rename = "playlistV2")]
    playlist: PathfinderPlaylist,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Content {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    items: Vec<Option<ContentItem>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentItem {
    #[serde(default)]
    item_v2: Option<Wrapper>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Millis {
    total_milliseconds: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderTrack {
    uri: String,
    name: String,
    #[serde(default)]
    track_duration: Option<Millis>,
    #[serde(default)]
    album_of_track: Option<PathfinderAlbum>,
    #[serde(default)]
    artists: Option<ArtistList>,
    #[serde(default)]
    disc_number: usize,
    #[serde(default)]
    track_number: usize,
    #[serde(default)]
    content_rating: Option<ContentRating>,
    #[serde(default)]
    playability: Option<Playability>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderAlbum {
    uri: String,
    name: String,
    #[serde(default)]
    cover_art: Option<Sources>,
}

#[derive(Deserialize)]
struct ArtistList {
    #[serde(default)]
    items: Vec<PathfinderArtist>,
}

#[derive(Deserialize)]
struct PathfinderArtist {
    uri: String,
    profile: ArtistProfile,
    /// Only in the library, for the followed artists.
    #[serde(default)]
    visuals: Option<ArtistVisuals>,
}

#[derive(Deserialize)]
struct ArtistProfile {
    name: String,
}

#[derive(Deserialize)]
struct ContentRating {
    label: String,
}

#[derive(Deserialize)]
struct Playability {
    playable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderLocalTrack {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    album_name: Option<String>,
    #[serde(default)]
    artist_name: Option<String>,
    #[serde(default)]
    local_track_duration: Option<Millis>,
}

impl PlaylistResponse {
    pub fn playlist(&self) -> Option<Playlist> {
        self.data.playlist.to_playlist(false)
    }

    pub fn total_count(&self) -> usize {
        self.data
            .playlist
            .content
            .as_ref()
            .map_or(0, |content| content.total_count)
    }

    /// Tracks of this page.  Local items are resolved against the local files
    /// index, episodes and unknown items are skipped.
    pub fn into_tracks(self, local_library: Option<&LocalLibrary>) -> Vec<Arc<Track>> {
        let Some(content) = self.data.playlist.content else {
            return Vec::new();
        };
        content
            .items
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let wrapper = item.item_v2?;
                let data = wrapper.data?;
                match wrapper.typename.as_str() {
                    "TrackResponseWrapper" => {
                        let track: PathfinderTrack = serde_json::from_value(data).ok()?;
                        Some(Arc::new(track.into_track()?))
                    }
                    "LocalTrackResponseWrapper" => {
                        let local: PathfinderLocalTrack = serde_json::from_value(data).ok()?;
                        let item = local.into_local_item();
                        Some(match local_library {
                            Some(library) => library.track_for_item(&item, 0, 0),
                            None => Arc::new(super::local_library::local_track(&item, None, 0, 0)),
                        })
                    }
                    _ => None,
                }
            })
            .collect()
    }
}

impl PathfinderTrack {
    fn into_track(self) -> Option<Track> {
        Some(Track {
            id: TrackId(ItemId::from_uri(&self.uri)?),
            name: self.name.into(),
            album: self.album_of_track.map(|album| AlbumLink {
                id: id_from_uri(&album.uri),
                name: album.name.into(),
                images: album
                    .cover_art
                    .as_ref()
                    .map(Sources::images)
                    .unwrap_or_default(),
            }),
            artists: self
                .artists
                .map(|artists| {
                    artists
                        .items
                        .into_iter()
                        .map(|artist| ArtistLink {
                            id: id_from_uri(&artist.uri),
                            name: artist.profile.name.into(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            duration: Duration::from_millis(
                self.track_duration.map_or(0, |d| d.total_milliseconds),
            ),
            disc_number: self.disc_number,
            track_number: self.track_number,
            explicit: self
                .content_rating
                .is_some_and(|rating| rating.label == "EXPLICIT"),
            is_local: false,
            local_path: None,
            is_playable: self.playability.map(|p| p.playable),
            popularity: None,
            track_pos: 0,
            lyrics: None,
        })
    }
}

impl PathfinderLocalTrack {
    fn into_local_item(self) -> LocalItem {
        // Prefer the explicit fields, fall back to what the URI encodes.
        let from_uri = self
            .uri
            .as_deref()
            .and_then(LocalItem::from_uri)
            .unwrap_or_default();
        let non_empty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        LocalItem {
            title: non_empty(self.name).unwrap_or(from_uri.title),
            artists: non_empty(self.artist_name)
                .map(|artist| vec![artist])
                .unwrap_or(from_uri.artists),
            album: non_empty(self.album_name).unwrap_or(from_uri.album),
            duration_ms: self
                .local_track_duration
                .map(|d| d.total_milliseconds)
                .filter(|ms| *ms > 0)
                .or(from_uri.duration_ms),
        }
    }
}

#[derive(Deserialize)]
pub struct AlbumResponse {
    data: AlbumData,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumData {
    album_union: PathfinderAlbumDetail,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderAlbumDetail {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    album_type: Option<String>,
    #[serde(default)]
    artists: Option<ArtistList>,
    #[serde(default)]
    cover_art: Option<Sources>,
    #[serde(default)]
    date: Option<AlbumDate>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    copyright: Option<CopyrightList>,
    #[serde(default)]
    tracks_v2: Option<AlbumTracks>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumDate {
    #[serde(default)]
    iso_string: Option<String>,
    #[serde(default)]
    precision: Option<String>,
}

#[derive(Deserialize)]
struct CopyrightList {
    #[serde(default)]
    items: Vec<PathfinderCopyright>,
}

#[derive(Deserialize)]
struct PathfinderCopyright {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumTracks {
    #[serde(default)]
    total_count: usize,
    #[serde(default)]
    items: Vec<AlbumTrackItem>,
}

#[derive(Deserialize)]
struct AlbumTrackItem {
    #[serde(default)]
    track: Option<PathfinderAlbumTrack>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathfinderAlbumTrack {
    uri: String,
    name: String,
    #[serde(default)]
    duration: Option<Millis>,
    #[serde(default)]
    artists: Option<ArtistList>,
    #[serde(default)]
    disc_number: usize,
    #[serde(default)]
    track_number: usize,
    #[serde(default)]
    content_rating: Option<ContentRating>,
    #[serde(default)]
    playability: Option<Playability>,
}

impl AlbumResponse {
    pub fn total_tracks(&self) -> usize {
        self.data
            .album_union
            .tracks_v2
            .as_ref()
            .map_or(0, |tracks| tracks.total_count)
    }

    pub fn page_len(&self) -> usize {
        self.data
            .album_union
            .tracks_v2
            .as_ref()
            .map_or(0, |tracks| tracks.items.len())
    }

    /// The tracks of this page, for appending the ones of later pages.
    pub fn into_tracks(self) -> Vec<Arc<Track>> {
        let Some(tracks) = self.data.album_union.tracks_v2 else {
            return Vec::new();
        };
        tracks
            .items
            .into_iter()
            .filter_map(|item| {
                let track = item.track?;
                Some(Arc::new(Track {
                    id: TrackId(ItemId::from_uri(&track.uri)?),
                    name: track.name.into(),
                    album: None,
                    artists: track
                        .artists
                        .map(|artists| {
                            artists
                                .items
                                .into_iter()
                                .map(|artist| ArtistLink {
                                    id: id_from_uri(&artist.uri),
                                    name: artist.profile.name.into(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    duration: Duration::from_millis(
                        track.duration.map_or(0, |d| d.total_milliseconds),
                    ),
                    disc_number: track.disc_number,
                    track_number: track.track_number,
                    explicit: track
                        .content_rating
                        .is_some_and(|rating| rating.label == "EXPLICIT"),
                    is_local: false,
                    local_path: None,
                    is_playable: track.playability.map(|p| p.playable),
                    popularity: None,
                    track_pos: 0,
                    lyrics: None,
                }))
            })
            .collect()
    }

    /// The album, with the tracks of this page.
    pub fn into_album(self) -> Option<Album> {
        let album = &self.data.album_union;
        if album.typename != "Album" {
            return None;
        }
        let id = id_from_uri(album.uri.as_deref()?);
        let name: Arc<str> = album.name.as_deref().unwrap_or_default().into();
        let album_type = match album.album_type.as_deref() {
            Some("SINGLE") => AlbumType::Single,
            Some("COMPILATION") => AlbumType::Compilation,
            _ => AlbumType::Album,
        };
        let images = album
            .cover_art
            .as_ref()
            .map(Sources::images)
            .unwrap_or_default();
        let artists = album
            .artists
            .as_ref()
            .map(|artists| {
                artists
                    .items
                    .iter()
                    .map(|artist| ArtistLink {
                        id: id_from_uri(&artist.uri),
                        name: artist.profile.name.as_str().into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let copyrights = album
            .copyright
            .as_ref()
            .map(|copyrights| {
                copyrights
                    .items
                    .iter()
                    .filter_map(|copyright| {
                        Some(Copyright {
                            text: copyright.text.as_deref()?.into(),
                            kind: match copyright.kind.as_deref() {
                                Some("P") => CopyrightType::Performance,
                                _ => CopyrightType::Copyright,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let label: Arc<str> = album.label.as_deref().unwrap_or_default().into();
        let (release_date, release_date_precision) = album
            .date
            .as_ref()
            .map(|date| {
                let parsed = date.iso_string.as_deref().and_then(parse_iso_date);
                let precision = match date.precision.as_deref() {
                    Some("DAY") => Some(DatePrecision::Day),
                    Some("MONTH") => Some(DatePrecision::Month),
                    Some("YEAR") => Some(DatePrecision::Year),
                    _ => None,
                };
                (parsed, precision)
            })
            .unwrap_or((None, None));
        let tracks = self.into_tracks().into_iter().collect();
        Some(Album {
            id,
            name,
            album_type,
            images,
            artists,
            copyrights,
            label,
            tracks,
            release_date,
            release_date_precision,
        })
    }
}

/// The date of `2024-02-16T00:00:00Z`.
fn parse_iso_date(iso: &str) -> Option<Date> {
    let mut parts = iso.get(..10)?.split('-');
    let year = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_album_from_pathfinder() {
        let response: AlbumResponse = serde_json::from_value(json!({
            "data": { "albumUnion": {
                "__typename": "Album",
                "uri": "spotify:album:1jXYc5gip5tqCTDOotfY5L",
                "name": "2093",
                "type": "ALBUM",
                "artists": { "items": [ { "uri": "spotify:artist:3qiHUAX7zY4Qnjx8TNUzVx", "profile": { "name": "Yeat" } } ] },
                "coverArt": { "sources": [ { "url": "https://i.scdn.co/image/a", "width": 640, "height": 640 } ] },
                "date": { "isoString": "2024-02-16T00:00:00Z", "precision": "DAY" },
                "label": "Lyfestyle Corporation",
                "copyright": { "items": [ { "type": "C", "text": "2024 Lyfestyle" }, { "type": "P", "text": "2024 Lyfestyle" } ] },
                "tracksV2": { "totalCount": 2, "items": [
                    { "uid": "a", "track": {
                        "uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
                        "name": "Psycho CEO",
                        "duration": { "totalMilliseconds": 152000 },
                        "artists": { "items": [ { "uri": "spotify:artist:3qiHUAX7zY4Qnjx8TNUzVx", "profile": { "name": "Yeat" } } ] },
                        "discNumber": 1, "trackNumber": 1,
                        "contentRating": { "label": "EXPLICIT" },
                        "playability": { "playable": true }
                    } },
                    { "uid": "b", "track": null }
                ] }
            } }
        }))
        .unwrap();
        assert_eq!(response.total_tracks(), 2);
        let album = response.into_album().unwrap();
        assert_eq!(&*album.id, "1jXYc5gip5tqCTDOotfY5L");
        assert_eq!(album.album_type, AlbumType::Album);
        assert_eq!(album.release(), "February 16, 2024");
        assert_eq!(&*album.label, "Lyfestyle Corporation");
        assert_eq!(album.copyrights.len(), 2);
        assert_eq!(album.tracks.len(), 1);
        assert!(album.tracks[0].explicit);
        assert_eq!(album.artists[0].name.as_ref(), "Yeat");
    }

    #[test]
    fn parses_library_with_pinned_playlists() {
        let response: LibraryResponse = serde_json::from_value(json!({
            "data": { "me": { "libraryV3": {
                "__typename": "LibraryPage",
                "totalCount": 3,
                "items": [
                    { "pinned": true, "item": {
                        "__typename": "PlaylistResponseWrapper",
                        "_uri": "spotify:playlist:37i9dQZF1E39abc",
                        "data": {
                            "__typename": "Playlist",
                            "uri": "spotify:playlist:37i9dQZF1E39abc",
                            "name": "Daily Mix 1",
                            "description": "Some <b>artists</b>",
                            "images": { "items": [ { "sources": [ { "url": "https://i.scdn.co/x", "width": 300, "height": 300 } ] } ] },
                            "ownerV2": { "data": { "__typename": "User", "name": "Spotify", "username": "spotify" } },
                            "currentUserCapabilities": { "canEditItems": false },
                            "revisionId": "abc"
                        }
                    } },
                    { "pinned": false, "item": {
                        "__typename": "FolderResponseWrapper",
                        "_uri": "spotify:user:me:folder:1",
                        "data": { "__typename": "Folder", "name": "Folder" }
                    } },
                    { "pinned": false, "item": {
                        "__typename": "PlaylistResponseWrapper",
                        "_uri": "spotify:playlist:mine",
                        "data": {
                            "__typename": "Playlist",
                            "uri": "spotify:playlist:mine",
                            "name": "Mine",
                            "ownerV2": { "data": { "__typename": "User", "name": "Me", "username": "me" } },
                            "currentUserCapabilities": { "canEditItems": true }
                        }
                    } }
                ]
            } } }
        }))
        .unwrap();
        assert_eq!(response.total_count(), 3);
        let playlists = response.into_playlists().unwrap();
        assert_eq!(playlists.len(), 2);
        assert_eq!(&*playlists[0].id, "37i9dQZF1E39abc");
        assert!(playlists[0].pinned);
        assert_eq!(&*playlists[0].owner.id, "spotify");
        assert_eq!(playlists[0].images.as_ref().unwrap().len(), 1);
        assert!(!playlists[1].pinned);
        assert!(playlists[1].collaborative);
    }

    #[test]
    fn parses_playlist_contents_with_local_tracks() {
        let response: PlaylistResponse = serde_json::from_value(json!({
            "data": { "playlistV2": {
                "__typename": "Playlist",
                "uri": "spotify:playlist:abc",
                "name": "Daily Mix 2",
                "ownerV2": { "data": { "name": "Spotify", "username": "spotify" } },
                "content": {
                    "totalCount": 3,
                    "items": [
                        { "itemV2": { "__typename": "TrackResponseWrapper", "_uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "data": {
                            "__typename": "Track",
                            "uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
                            "name": "Song",
                            "trackDuration": { "totalMilliseconds": 200000 },
                            "albumOfTrack": { "uri": "spotify:album:1", "name": "Album", "coverArt": { "sources": [ { "url": "https://i.scdn.co/a" } ] } },
                            "artists": { "items": [ { "uri": "spotify:artist:2", "profile": { "name": "Artist" } } ] },
                            "discNumber": 1, "trackNumber": 3,
                            "contentRating": { "label": "EXPLICIT" },
                            "playability": { "playable": true }
                        } } },
                        { "itemV2": { "__typename": "LocalTrackResponseWrapper", "_uri": "spotify:local:A:B:Local+Song:100", "data": {
                            "__typename": "LocalTrack",
                            "uri": "spotify:local:A:B:Local+Song:100",
                            "name": "Local Song",
                            "albumName": "B",
                            "artistName": "A",
                            "localTrackDuration": { "totalMilliseconds": 100000 }
                        } } },
                        { "itemV2": { "__typename": "EpisodeOrChapterResponseWrapper", "data": { "__typename": "Episode" } } },
                        null
                    ]
                }
            } }
        }))
        .unwrap();
        let playlist = response.playlist().unwrap();
        assert_eq!(&*playlist.name, "Daily Mix 2");
        assert_eq!(playlist.track_count, Some(3));

        let tracks = response.into_tracks(None);
        assert_eq!(tracks.len(), 2);
        assert_eq!(&*tracks[0].name, "Song");
        assert!(tracks[0].explicit);
        assert_eq!(tracks[0].artist_name().as_ref(), "Artist");
        assert_eq!(tracks[0].duration, Duration::from_secs(200));
        assert!(tracks[1].is_local);
        assert!(tracks[1].is_missing_local_file());
        assert_eq!(&*tracks[1].name, "Local Song");
    }
}
