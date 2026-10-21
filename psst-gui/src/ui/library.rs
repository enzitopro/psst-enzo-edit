use std::sync::Arc;

use druid::{
    im::Vector,
    widget::{Flex, List},
    LensExt, Selector, Widget, WidgetExt,
};

use crate::{
    cmd,
    data::{
        Album, AlbumLink, AppState, Ctx, Library, Nav, PlaylistLink, SavedAlbums, SavedTracks,
        Show, ShowLink, Track, TrackId,
    },
    error::Error,
    ui::home::{shows_that_you_might_like, your_shows},
    webapi::WebApi,
    widget::{Async, Empty, MyWidgetExt},
};

use super::{album, artist, playable, track, utils};

pub const LOAD_TRACKS: Selector = Selector::new("app.library.load-tracks");
pub const LOAD_ALBUMS: Selector = Selector::new("app.library.load-albums");
pub const LOAD_ARTISTS: Selector = Selector::new("app.library.load-artists");
pub const LOAD_SHOWS: Selector = Selector::new("app.library.load-shows");

pub const SAVE_TRACK: Selector<Arc<Track>> = Selector::new("app.library.save-track");
/// Like every track of a playlist or an album.
pub const LIKE_PLAYLIST: Selector<PlaylistLink> = Selector::new("app.library.like-playlist");
pub const LIKE_ALBUM: Selector<AlbumLink> = Selector::new("app.library.like-album");
pub const UNSAVE_TRACK: Selector<TrackId> = Selector::new("app.library.unsave-track");

pub const SAVE_ALBUM: Selector<Arc<Album>> = Selector::new("app.library.save-album");
pub const UNSAVE_ALBUM: Selector<AlbumLink> = Selector::new("app.library.unsave-album");

pub const SAVE_SHOW: Selector<Arc<Show>> = Selector::new("app.library.save-show");
pub const UNSAVE_SHOW: Selector<ShowLink> = Selector::new("app.library.unsave-show");

pub fn saved_tracks_widget() -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        || {
            playable::list_widget_with_find(
                playable::Display {
                    track: track::Display {
                        title: true,
                        artist: true,
                        album: true,
                        cover: true,
                        ..track::Display::empty()
                    },
                },
                cmd::FIND_IN_SAVED_TRACKS,
            )
        },
        utils::error_widget,
    )
    .lens(
        Ctx::make(
            AppState::common_ctx,
            AppState::library.then(Library::saved_tracks.in_arc()),
        )
        .then(Ctx::in_promise()),
    )
}

/// The artists the user follows; each opens their saved songs.
pub fn followed_artists_widget() -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        || List::new(|| artist::artist_widget(false, Nav::ArtistSavedTracks)),
        utils::error_widget,
    )
    .lens(AppState::library.then(Library::followed_artists.in_arc()))
    .on_command_async(
        LOAD_ARTISTS,
        |_| WebApi::global().get_followed_artists(),
        |_, data, _| data.with_library_mut(|library| library.followed_artists.defer_default()),
        |_, data, r| data.with_library_mut(|library| library.followed_artists.update(r)),
    )
}

/// Likes the fetched tracks, leaving out local files (they aren't on Spotify).
fn like_all(tracks: Result<Vector<Arc<Track>>, Error>) -> Result<Vector<Arc<Track>>, Error> {
    let tracks: Vector<_> = tracks?.into_iter().filter(|t| !t.is_local).collect();
    WebApi::global().save_tracks(&tracks.iter().cloned().collect::<Vec<_>>())?;
    Ok(tracks)
}

fn liked_all(data: &mut AppState, result: Result<Vector<Arc<Track>>, Error>) {
    match result {
        Ok(tracks) => {
            let mut added = 0;
            data.with_library_mut(|library| added = library.add_tracks(tracks));
            data.info_alert(format!("Added {added} songs to Liked Songs"));
        }
        Err(err) => data.error_alert(format!("Couldn't like the songs: {err}")),
    }
}

/// Loads and changes the saved tracks.  Shows nothing, but is always there: the
/// Saved Tracks page isn't, so liking a track from another page used to do nothing.
pub fn saved_tracks_loader() -> impl Widget<AppState> {
    Empty
        .on_command_async(
            LIKE_PLAYLIST,
            |link| like_all(WebApi::global().get_playlist_tracks(&link.id)),
            |_, data: &mut AppState, link| {
                data.info_alert(format!("Adding \"{}\" to Liked Songs…", link.name))
            },
            |_, data, (_, result)| liked_all(data, result),
        )
        .on_command_async(
            LIKE_ALBUM,
            |link| {
                like_all(
                    WebApi::global()
                        .get_album(&link.id)
                        .map(|a| a.data.tracks.clone()),
                )
            },
            |_, data, link| data.info_alert(format!("Adding \"{}\" to Liked Songs…", link.name)),
            |_, data, (_, result)| liked_all(data, result),
        )
        .on_command_async(
            LOAD_TRACKS,
            |_| WebApi::global().get_saved_tracks().map(SavedTracks::new),
            |_, data: &mut AppState, _| {
                data.with_library_mut(|library| {
                    library.saved_tracks.defer_default();
                });
            },
            |_, data, r| {
                data.with_library_mut(|library| {
                    library.saved_tracks.update(r);
                });
                data.resort_tracks();
            },
        )
        .on_command_async(
            SAVE_TRACK,
            |t| WebApi::global().save_track(&t.id.0.to_base62()),
            |_, data, t| {
                data.with_library_mut(|library| {
                    library.add_track(t);
                });
                data.resort_tracks();
            },
            |_, data, (_, r)| {
                if let Err(err) = r {
                    data.error_alert(err);
                } else {
                    data.info_alert("Track added to library.")
                }
            },
        )
        .on_command_async(
            UNSAVE_TRACK,
            |i| WebApi::global().unsave_track(&i.0.to_base62()),
            |_, data, i| {
                data.with_library_mut(|library| {
                    library.remove_track(&i);
                });
            },
            |_, data, (_, r)| {
                if let Err(err) = r {
                    data.error_alert(err);
                } else {
                    data.info_alert("Track removed from library.")
                }
            },
        )
}

pub fn saved_albums_widget() -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        || List::new(|| album::album_widget(false)).lens(Ctx::map(SavedAlbums::albums)),
        utils::error_widget,
    )
    .lens(
        Ctx::make(
            AppState::common_ctx,
            AppState::library.then(Library::saved_albums.in_arc()),
        )
        .then(Ctx::in_promise()),
    )
    .on_command_async(
        LOAD_ALBUMS,
        |_| WebApi::global().get_saved_albums().map(SavedAlbums::new),
        |_, data, _| {
            data.with_library_mut(|library| {
                library.saved_albums.defer_default();
            });
        },
        |_, data, r| {
            data.with_library_mut(|library| {
                library.saved_albums.update(r);
            });
        },
    )
    .on_command_async(
        SAVE_ALBUM,
        |a| WebApi::global().save_album(&a.id),
        |_, data, a| {
            data.with_library_mut(move |library| {
                library.add_album(a);
            });
        },
        |_, data, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Album added to library.");
            }
        },
    )
    .on_command_async(
        UNSAVE_ALBUM,
        |l| WebApi::global().unsave_album(&l.id),
        |_, data, l| {
            data.with_library_mut(|library| {
                library.remove_album(&l.id);
            });
        },
        |_, data, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Album removed from library.");
            }
        },
    )
}

pub fn saved_shows_widget() -> impl Widget<AppState> {
    Flex::column()
        .with_child(your_shows())
        .with_child(shows_that_you_might_like())
}
