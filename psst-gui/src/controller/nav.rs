use std::time::Duration;

use crate::{
    cmd,
    data::{AppState, Nav, PromiseState, SpotifyUrl},
    ui::{album, artist, home, library, lyrics, playlist, search, show, stats},
    webapi::WebApi,
};
use druid::widget::{prelude::*, Controller};
use druid::Code;

pub struct NavController;

impl NavController {
    /// Ctrl+R and the Retry buttons: the page again, fetched even when cached,
    /// and the sidebar's playlists if they failed.
    fn reload(&self, ctx: &mut EventCtx, data: &mut AppState) {
        let api = WebApi::global();
        match &data.nav {
            Nav::AlbumDetail(link, _) => api.refresh_cached("album", &link.id),
            Nav::ArtistDetail(link) => {
                api.refresh_cached("artist", &link.id);
                api.refresh_cached("artist-overview", &link.id);
            }
            Nav::ShowDetail(link) => api.refresh_cached("show", &link.id),
            _ => {}
        }
        data.refresh_all();
        if data.library.playlists.state() == PromiseState::Rejected {
            ctx.submit_command(playlist::LOAD_LIST.with(data.config.playlist_order.clone()));
        }
        // Loaded pages of the library stay loaded otherwise.
        match data.nav {
            Nav::Home => ctx.submit_command(home::LOAD_MADE_FOR_YOU),
            Nav::SavedTracks => ctx.submit_command(library::LOAD_TRACKS),
            Nav::SavedAlbums => ctx.submit_command(library::LOAD_ALBUMS),
            Nav::Shows => ctx.submit_command(library::LOAD_SHOWS),
            _ => self.load_route_data(ctx, data),
        }
    }

    fn load_route_data(&self, ctx: &mut EventCtx, data: &mut AppState) {
        match &data.nav {
            Nav::Home => {}
            // One request each: fresh on every visit.
            Nav::NewReleases => {
                if !data.new_releases.is_deferred(&()) {
                    ctx.submit_command(library::LOAD_NEW_RELEASES);
                }
            }
            Nav::Stats => {
                if !data.stats.is_deferred(&data.stats_period) {
                    ctx.submit_command(stats::LOAD_STATS.with(data.stats_period.clone()));
                }
            }
            Nav::Lyrics => {}
            Nav::SavedTracks => {
                // Liked on the phone meanwhile?  4000 tracks take a while, so not
                // on every visit.
                const REFRESH_AFTER: Duration = Duration::from_secs(10 * 60);
                let stale = data
                    .library
                    .saved_tracks
                    .resolved()
                    .is_some_and(|saved| saved.loaded_at.elapsed() > REFRESH_AFTER);
                if data.library.saved_tracks.needs_loading() || stale {
                    ctx.submit_command(library::LOAD_TRACKS);
                }
            }
            Nav::SavedAlbums => {
                if data.library.saved_albums.needs_loading() {
                    ctx.submit_command(library::LOAD_ALBUMS);
                }
            }
            // A couple of requests: refreshed on every visit.
            Nav::SavedArtists => {
                if data.library.followed_artists.state() != PromiseState::Deferred {
                    ctx.submit_command(library::LOAD_ARTISTS);
                }
            }
            Nav::ArtistSavedTracks(_) => data.refresh_artist_saved_tracks(),
            Nav::Shows => {
                if data.library.saved_shows.needs_loading() {
                    ctx.submit_command(library::LOAD_SHOWS);
                }
            }
            Nav::SearchResults(query) => {
                if let Some(link) = SpotifyUrl::parse(query) {
                    ctx.submit_command(search::OPEN_LINK.with(link));
                } else if !data
                    .search
                    .results
                    .contains(&(query.clone(), data.search.topic))
                {
                    ctx.submit_command(
                        search::LOAD_RESULTS.with((query.to_owned(), data.search.topic)),
                    );
                }
            }
            Nav::AlbumDetail(link, _) => {
                if !data.album_detail.album.contains(link) {
                    ctx.submit_command(album::LOAD_DETAIL.with(link.to_owned()));
                }
            }
            Nav::ArtistDetail(link) => {
                // For the Follow button.
                if data.library.followed_artists.needs_loading() {
                    ctx.submit_command(library::LOAD_ARTISTS);
                }
                if !data.artist_detail.artist.contains(link) {
                    ctx.submit_command(artist::LOAD_DETAIL.with(link.to_owned()));
                }
            }
            Nav::PlaylistDetail(link) => {
                if !data.playlist_detail.playlist.contains(link) {
                    ctx.submit_command(
                        playlist::LOAD_DETAIL.with((link.to_owned(), data.to_owned())),
                    );
                }
            }
            Nav::ShowDetail(link) => {
                if !data.show_detail.show.contains(link) {
                    ctx.submit_command(show::LOAD_DETAIL.with(link.to_owned()));
                }
            }
        }
    }
}

impl<W> Controller<AppState, W> for NavController
where
    W: Widget<AppState>,
{
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut AppState,
        env: &Env,
    ) {
        match event {
            Event::Command(cmd) if cmd.is(cmd::NAVIGATE) => {
                let nav = cmd.get_unchecked(cmd::NAVIGATE);
                data.navigate(nav);
                ctx.set_handled();
                self.load_route_data(ctx, data);
            }
            Event::Command(cmd) if cmd.is(cmd::NAVIGATE_BACK) => {
                let count = cmd.get_unchecked(cmd::NAVIGATE_BACK);
                for _ in 0..*count {
                    data.navigate_back();
                }
                ctx.set_handled();
                self.load_route_data(ctx, data);
            }
            Event::Command(cmd) if cmd.is(cmd::NAVIGATE_REFRESH) => {
                data.refresh_playlist();
                ctx.set_handled();
                self.load_route_data(ctx, data);
            }
            Event::Command(cmd) if cmd.is(cmd::TOGGLE_LYRICS) => {
                match data.nav {
                    Nav::Lyrics => data.navigate_back(),
                    _ => {
                        data.navigate(&Nav::Lyrics);
                        if let Some(np) = data.playback.now_playing.as_ref() {
                            ctx.submit_command(lyrics::SHOW_LYRICS.with(np.clone()));
                        }
                    }
                }
                ctx.set_handled();
                self.load_route_data(ctx, data);
            }
            Event::MouseDown(cmd) if cmd.button.is_x1() => {
                data.navigate_back();
                ctx.set_handled();
                self.load_route_data(ctx, data);
            }
            Event::KeyDown(key) if key.mods.ctrl() && key.code == Code::KeyR => {
                ctx.set_handled();
                self.reload(ctx, data);
            }
            Event::Command(cmd) if cmd.is(cmd::RELOAD) => {
                ctx.set_handled();
                self.reload(ctx, data);
            }
            _ => {
                child.event(ctx, event, data, env);
            }
        }
    }

    fn lifecycle(
        &mut self,
        child: &mut W,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &AppState,
        env: &Env,
    ) {
        if let LifeCycle::WidgetAdded = event {
            // Not through the Tracks page first, to load the saved tracks: pages stay
            // built once visited, and 4000 rows took ~170 MB.  The session loads them.
            // Load the last route, or the default.
            ctx.submit_command(
                cmd::NAVIGATE.with(data.config.last_route.to_owned().unwrap_or_default()),
            );
        }
        child.lifecycle(ctx, event, data, env)
    }
}
