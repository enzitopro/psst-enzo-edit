use std::sync::Arc;

use druid::Data;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::data::track::TrackId;
use crate::data::{AlbumLink, ArtistLink, PlaylistLink, ShowLink};

#[derive(Copy, Clone, Debug, Data, PartialEq, Eq, Hash)]
pub enum Route {
    Home,
    Lyrics,
    SavedTracks,
    SavedAlbums,
    SavedArtists,
    Shows,
    SearchResults,
    ArtistDetail,
    ArtistSavedTracks,
    AlbumDetail,
    ShowDetail,
    PlaylistDetail,
}

#[derive(Clone, Debug, Data, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Nav {
    #[default]
    Home,
    Lyrics,
    SavedTracks,
    SavedAlbums,
    SavedArtists,
    Shows,
    SearchResults(Arc<str>),
    AlbumDetail(AlbumLink, Option<TrackId>),
    ArtistDetail(ArtistLink),
    /// The artist's tracks in the library.
    ArtistSavedTracks(ArtistLink),
    PlaylistDetail(PlaylistLink),
    ShowDetail(ShowLink),
}

pub const SAVED_TRACKS_LIST: &str = "saved-tracks";

impl Nav {
    /// The key of the sortable track list on this page, for its sort.
    pub fn track_list(&self) -> Option<&str> {
        match self {
            Nav::SavedTracks => Some(SAVED_TRACKS_LIST),
            Nav::PlaylistDetail(link) => Some(&link.id),
            // An artist's saved songs.
            Nav::ArtistSavedTracks(link) => Some(&link.id),
            _ => None,
        }
    }

    pub fn route(&self) -> Route {
        match self {
            Nav::Home => Route::Home,
            Nav::Lyrics => Route::Lyrics,
            Nav::SavedTracks => Route::SavedTracks,
            Nav::SavedAlbums => Route::SavedAlbums,
            Nav::SavedArtists => Route::SavedArtists,
            Nav::Shows => Route::Shows,
            Nav::SearchResults(_) => Route::SearchResults,
            Nav::AlbumDetail(_, _) => Route::AlbumDetail,
            Nav::ArtistDetail(_) => Route::ArtistDetail,
            Nav::ArtistSavedTracks(_) => Route::ArtistSavedTracks,
            Nav::PlaylistDetail(_) => Route::PlaylistDetail,
            Nav::ShowDetail(_) => Route::ShowDetail,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Nav::Home => "Home".to_string(),
            Nav::Lyrics => "Lyrics".to_string(),
            Nav::SavedTracks => "Saved Tracks".to_string(),
            Nav::SavedAlbums => "Saved Albums".to_string(),
            Nav::SavedArtists => "Artists".to_string(),
            Nav::Shows => "Podcasts".to_string(),
            Nav::SearchResults(query) => query.to_string(),
            Nav::AlbumDetail(link, _) => link.name.to_string(),
            Nav::ArtistDetail(link) => link.name.to_string(),
            Nav::ArtistSavedTracks(link) => link.name.to_string(),
            Nav::PlaylistDetail(link) => link.name.to_string(),
            Nav::ShowDetail(link) => link.name.to_string(),
        }
    }

    pub fn full_title(&self) -> String {
        match self {
            Nav::Home => "Home".to_string(),
            Nav::Lyrics => "Lyrics".to_string(),
            Nav::SavedTracks => "Saved Tracks".to_string(),
            Nav::SavedAlbums => "Saved Albums".to_string(),
            Nav::SavedArtists => "Followed Artists".to_string(),
            Nav::Shows => "Saved Shows".to_string(),
            Nav::SearchResults(query) => format!("Search \"{query}\""),
            Nav::AlbumDetail(link, _) => format!("Album \"{}\"", link.name),
            Nav::ArtistDetail(link) => format!("Artist \"{}\"", link.name),
            Nav::ArtistSavedTracks(link) => format!("Saved songs by \"{}\"", link.name),
            Nav::PlaylistDetail(link) => format!("Playlist \"{}\"", link.name),
            Nav::ShowDetail(link) => format!("Show \"{}\"", link.name),
        }
    }
}

#[derive(Clone, Debug, Data, Eq, PartialEq, Hash)]
pub enum SpotifyUrl {
    Playlist(Arc<str>),
    Artist(Arc<str>),
    Album(Arc<str>),
    Track(Arc<str>),
    Show(Arc<str>),
}

impl SpotifyUrl {
    pub fn parse(url: &str) -> Option<Self> {
        let url = Url::parse(url).ok()?;
        let mut segments = url.path_segments()?;
        let entity = segments.next()?;
        let id = segments.next()?;
        match entity {
            "playlist" => Some(Self::Playlist(id.into())),
            "artist" => Some(Self::Artist(id.into())),
            "album" => Some(Self::Album(id.into())),
            "track" => Some(Self::Track(id.into())),
            "show" => Some(Self::Show(id.into())),
            _ => None,
        }
    }

    pub fn id(&self) -> Arc<str> {
        match self {
            SpotifyUrl::Playlist(id) => id.clone(),
            SpotifyUrl::Artist(id) => id.clone(),
            SpotifyUrl::Album(id) => id.clone(),
            SpotifyUrl::Track(id) => id.clone(),
            SpotifyUrl::Show(id) => id.clone(),
        }
    }
}
