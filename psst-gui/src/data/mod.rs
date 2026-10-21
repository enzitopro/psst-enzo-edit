mod album;
mod artist;
pub mod config;
mod ctx;
mod find;
mod id;
mod lyrics;
mod nav;
mod playback;
mod playlist;
mod promise;
mod search;
mod show;
mod slider_scroll_scale;
mod track;
mod user;
pub mod utils;

use std::{
    fmt::Display,
    fs, mem,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use druid::{
    im::{HashMap, HashSet, Vector},
    Data, Lens,
};
use psst_core::{item_id::ItemId, session::SessionService};

pub use crate::data::{
    album::{Album, AlbumDetail, AlbumLink, AlbumType, Copyright, CopyrightType, DatePrecision},
    artist::{
        Artist, ArtistAlbums, ArtistDetail, ArtistInfo, ArtistLink, ArtistOverview, ArtistStats,
    },
    config::{
        AudioQuality, Authentication, Config, Preferences, PreferencesTab, SortCriteria,
        SortOrder, Theme,
    },
    ctx::Ctx,
    find::{FindQuery, Finder, MatchFindQuery},
    lyrics::{Lyrics, LyricsCandidate, LyricsSearch},
    nav::{Nav, Route, SpotifyUrl},
    playback::{
        NowPlaying, Playable, PlayableMatcher, Playback, PlaybackOrigin, PlaybackPayload,
        PlaybackState, QueueBehavior, QueueEntry, SavedPlayback,
    },
    playlist::{
        Playlist, PlaylistAddTrack, PlaylistDetail, PlaylistLink, PlaylistRemoveTrack,
        PlaylistTracks,
    },
    promise::{Promise, PromiseState},
    search::{Search, SearchResults, SearchTopic},
    show::{Episode, EpisodeId, EpisodeLink, Show, ShowDetail, ShowEpisodes, ShowLink},
    slider_scroll_scale::SliderScrollScale,
    track::{sort_tracks, AudioAnalysis, Track, TrackId, TrackLines},
    user::{PublicUser, UserProfile},
    utils::{Cached, Image, Page},
};
use crate::ui::credits::TrackCredits;

pub const ALERT_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Data, Lens)]
pub struct AppState {
    #[data(ignore)]
    pub session: SessionService,
    pub nav: Nav,
    pub history: Vector<Nav>,
    pub config: Config,
    pub preferences: Preferences,
    pub playback: Playback,
    pub search: Search,
    pub album_detail: AlbumDetail,
    pub artist_detail: ArtistDetail,
    pub playlist_detail: PlaylistDetail,
    pub show_detail: ShowDetail,
    pub library: Arc<Library>,
    pub common_ctx: Arc<CommonCtx>,
    pub home_detail: HomeDetail,
    pub alerts: Vector<Alert>,
    pub finder: Finder,
    pub added_queue: Vector<QueueEntry>,
    pub lyrics: Promise<Lyrics>,
    pub lyrics_search: LyricsSearch,
    /// The small lyrics panel over the main window.
    pub lyrics_panel_open: bool,
    pub credits: Option<TrackCredits>,
    /// Offline downloads still to do, in download order, and how many were
    /// queued since the queue was last empty.
    pub download_queue: Vector<Arc<Track>>,
    pub download_total: usize,
}

impl AppState {
    pub fn default_with_config(config: Config) -> Self {
        let library = Arc::new(Library {
            user_profile: Promise::Empty,
            saved_albums: Promise::Empty,
            saved_tracks: Promise::Empty,
            saved_shows: Promise::Empty,
            followed_artists: Promise::Empty,
            playlists: Promise::Empty,
        });
        let common_ctx = Arc::new(CommonCtx {
            now_playing: None,
            library: Arc::clone(&library),
            show_track_cover: config.show_track_cover,
            nav: Nav::Home,
            downloaded: HashSet::new(),
            downloading: HashSet::new(),
            offline_playlists: HashMap::new(),
        });
        let playback = Playback {
            state: PlaybackState::Stopped,
            now_playing: None,
            queue_behavior: config.queue_behavior,
            queue: Vector::new(),
            volume: config.volume,
        };
        Self {
            session: SessionService::empty(),
            nav: Nav::Home,
            history: Vector::new(),
            config,
            preferences: Preferences {
                active: PreferencesTab::General,
                cache: None,
                cache_size: Promise::Empty,
                auth: Authentication::new(),
                lastfm_auth_result: None,
                local_files: Default::default(),
            },
            playback,
            added_queue: Vector::new(),
            search: Search {
                input: "".into(),
                topic: None,
                results: Promise::Empty,
            },
            home_detail: HomeDetail {
                made_for_you: Promise::Empty,
                user_top_mixes: Promise::Empty,
                best_of_artists: Promise::Empty,
                recommended_stations: Promise::Empty,
                your_shows: Promise::Empty,
                shows_that_you_might_like: Promise::Empty,
                uniquely_yours: Promise::Empty,
                jump_back_in: Promise::Empty,
                user_top_tracks: Promise::Empty,
                user_top_artists: Promise::Empty,
            },
            album_detail: AlbumDetail {
                album: Promise::Empty,
            },
            artist_detail: ArtistDetail {
                artist: Promise::Empty,
                albums: Promise::Empty,
                overview: Promise::Empty,
                saved_tracks: Vector::new(),
            },
            playlist_detail: PlaylistDetail {
                playlist: Promise::Empty,
                tracks: Promise::Empty,
            },
            show_detail: ShowDetail {
                show: Promise::Empty,
                episodes: Promise::Empty,
            },
            library,
            common_ctx,
            alerts: Vector::new(),
            finder: Finder::new(),
            lyrics: Promise::Empty,
            lyrics_search: LyricsSearch::default(),
            lyrics_panel_open: false,
            credits: None,
            download_queue: Vector::new(),
            download_total: 0,
        }
    }
}

impl AppState {
    pub fn navigate(&mut self, nav: &Nav) {
        if &self.nav != nav {
            let previous = mem::replace(&mut self.nav, nav.to_owned());
            self.history.push_back(previous);
            self.config.last_route.replace(nav.to_owned());
            Arc::make_mut(&mut self.common_ctx).nav = nav.to_owned();
        }
    }

    pub fn navigate_back(&mut self) {
        if let Some(mut nav) = self.history.pop_back() {
            if let Nav::SearchResults(query) = &nav {
                if SpotifyUrl::parse(query).is_some() {
                    nav = self.history.pop_back().unwrap_or(Nav::Home);
                }
            }

            if let Nav::AlbumDetail(album, _) = nav {
                nav = Nav::AlbumDetail(album, None);
            }

            self.nav = nav;
            self.config.last_route.replace(self.nav.to_owned());
            Arc::make_mut(&mut self.common_ctx).nav = self.nav.clone();
        }
    }

    pub fn refresh_all(&mut self) {
        self.album_detail.album = Promise::Empty;
        self.artist_detail.overview = Promise::Empty;
        self.artist_detail.albums = Promise::Empty;
        self.artist_detail.artist = Promise::Empty;
        self.playlist_detail.playlist = Promise::Empty;
        self.playlist_detail.tracks = Promise::Empty;
        self.show_detail.episodes = Promise::Empty;
        self.show_detail.show = Promise::Empty;
    }

    pub fn refresh_playlist(&mut self) {
        self.playlist_detail.tracks = Promise::Empty;
        self.playlist_detail.playlist = Promise::Empty;
    }

    /// The sort of the track list on screen.
    pub fn current_sort(&self) -> (SortCriteria, SortOrder) {
        let list = self.nav.track_list();
        list.map(|list| self.config.track_sort(list))
            .unwrap_or_default()
    }

    /// Re-sorts the open playlist and the saved tracks in place, each by its
    /// own sort, after one changes.  No need to load them again.
    pub fn resort_tracks(&mut self) {
        if let Some(playlist) = self.playlist_detail.tracks.resolved_mut() {
            let (criteria, order) = self.config.track_sort(&playlist.id);
            playlist.tracks = sort_tracks(&playlist.tracks, criteria, order);
        }
        if self.library.saved_tracks.is_resolved() {
            let (criteria, order) = self.config.track_sort(nav::SAVED_TRACKS_LIST);
            self.with_library_mut(|library| {
                if let Some(saved) = library.saved_tracks.resolved_mut() {
                    saved.tracks = sort_tracks(&saved.tracks, criteria, order);
                }
            });
        }
    }
}

impl AppState {
    pub fn queued_entry(&self, item_id: ItemId) -> Option<QueueEntry> {
        if let Some(queued) = self
            .playback
            .queue
            .iter()
            .find(|queued| queued.item.id() == item_id)
            .cloned()
        {
            Some(queued)
        } else {
            self.added_queue
                .iter()
                .find(|queued| queued.item.id() == item_id)
                .cloned()
        }
    }

    pub fn add_queued_entry(&mut self, queue_entry: QueueEntry) {
        self.added_queue.push_back(queue_entry);
    }

    pub fn loading_playback(&mut self, item: Playable, origin: PlaybackOrigin) {
        self.common_ctx_mut().now_playing.take();
        self.playback.state = PlaybackState::Loading;
        self.playback.now_playing.replace(NowPlaying {
            item,
            origin,
            progress: Duration::default(),
            library: Arc::clone(&self.library),
        });
    }

    pub fn start_playback(&mut self, item: Playable, origin: PlaybackOrigin, progress: Duration) {
        let playing = (item.clone(), origin.clone());
        self.common_ctx_mut().now_playing.replace(playing);
        self.playback.state = PlaybackState::Playing;
        self.playback.now_playing.replace(NowPlaying {
            item,
            origin,
            progress,
            library: Arc::clone(&self.library),
        });
    }

    pub fn progress_playback(&mut self, progress: Duration) {
        if let Some(now_playing) = &mut self.playback.now_playing {
            now_playing.progress = progress;
        }
    }

    pub fn pause_playback(&mut self) {
        self.playback.state = PlaybackState::Paused;
    }

    pub fn resume_playback(&mut self) {
        self.playback.state = PlaybackState::Playing;
    }

    pub fn block_playback(&mut self) {
        // TODO: Figure out how to signal blocked playback properly.
    }

    pub fn stop_playback(&mut self) {
        self.playback.state = PlaybackState::Stopped;
        self.playback.now_playing.take();
        self.common_ctx_mut().now_playing.take();
    }

    pub fn set_queue_behavior(&mut self, queue_behavior: QueueBehavior) {
        self.playback.queue_behavior = queue_behavior;
        self.config.queue_behavior = queue_behavior;
        self.config.save();
    }
}

impl AppState {
    pub fn common_ctx_mut(&mut self) -> &mut CommonCtx {
        Arc::make_mut(&mut self.common_ctx)
    }

    pub fn with_library_mut(&mut self, func: impl FnOnce(&mut Library)) {
        func(Arc::make_mut(&mut self.library));
        self.library_updated();
        self.refresh_artist_saved_tracks();
    }

    /// The open artist's tracks in the library, for their saved songs page.
    pub fn refresh_artist_saved_tracks(&mut self) {
        self.artist_detail.saved_tracks = match (&self.nav, self.library.saved_tracks.resolved()) {
            (Nav::ArtistSavedTracks(artist), Some(saved)) => saved
                .tracks
                .iter()
                .filter(|track| track.artists.iter().any(|a| a.id == artist.id))
                .cloned()
                .collect(),
            _ => Vector::new(),
        };
    }

    fn library_updated(&mut self) {
        if let Some(now_playing) = &mut self.playback.now_playing {
            now_playing.library = Arc::clone(&self.library);
        }
        self.common_ctx_mut().library = Arc::clone(&self.library);
    }
}

impl AppState {
    /// Marks tracks as downloading, skipping ones already downloaded or queued.
    pub fn queue_downloads(&mut self, tracks: &Vector<Arc<Track>>) {
        for track in tracks {
            let common = self.common_ctx_mut();
            if !common.downloaded.contains(&track.id)
                && common.downloading.insert(track.id).is_none()
            {
                self.download_queue.push_back(track.clone());
                self.download_total += 1;
            }
        }
    }

    /// A track is in the cache now (downloaded or streamed), or failed to download.
    pub fn finish_download(&mut self, id: TrackId, ok: bool) {
        let common = self.common_ctx_mut();
        common.downloading.remove(&id);
        if ok {
            common.downloaded.insert(id);
        }
        self.download_queue.retain(|t| t.id != id);
        if self.download_queue.is_empty() {
            self.download_total = 0;
        }
    }

    pub fn add_alert(&mut self, message: impl Display, style: AlertStyle) {
        let alert = Alert {
            message: message.to_string().into(),
            style,
            id: Alert::fresh_id(),
            created_at: Instant::now(),
        };
        self.alerts.push_back(alert);
    }

    pub fn info_alert(&mut self, message: impl Display) {
        self.add_alert(message, AlertStyle::Info);
    }

    pub fn error_alert(&mut self, message: impl Display) {
        self.add_alert(message, AlertStyle::Error);
    }

    pub fn dismiss_alert(&mut self, id: usize) {
        self.alerts.retain(|a| a.id != id);
    }

    pub fn cleanup_alerts(&mut self) {
        let now = Instant::now();
        self.alerts
            .retain(|alert| now.duration_since(alert.created_at) < ALERT_DURATION);
    }
}

#[derive(Clone, Data, Lens)]
pub struct Library {
    pub user_profile: Promise<UserProfile>,
    pub playlists: Promise<Vector<Playlist>>,
    pub saved_albums: Promise<SavedAlbums>,
    pub saved_tracks: Promise<SavedTracks>,
    pub saved_shows: Promise<Shows>,
    pub followed_artists: Promise<Vector<Artist>>,
}

impl Library {
    pub fn add_track(&mut self, track: Arc<Track>) {
        self.add_tracks([track]);
    }

    /// Puts `tracks` at the top of the saved tracks, in their order, skipping the
    /// ones already saved.  Returns how many were added.
    pub fn add_tracks(&mut self, tracks: impl IntoIterator<Item = Arc<Track>>) -> usize {
        let Some(saved) = self.saved_tracks.resolved_mut() else {
            return 0;
        };
        // Also skips repeats, a playlist can have the same track twice.
        let mut seen = saved.set.clone();
        let new: Vec<_> = tracks
            .into_iter()
            .filter(|track| seen.insert(track.id).is_none())
            .collect();
        // Newest first in "date added" order.  The others aren't renumbered (that
        // copied all 4000 of them): the sort is stable, so a tie at 0 keeps the new
        // ones on top.
        let added = new.len();
        for mut track in new.into_iter().rev() {
            Arc::make_mut(&mut track).track_pos = 0;
            saved.set.insert(track.id);
            saved.tracks.push_front(track);
        }
        added
    }

    pub fn remove_track(&mut self, track_id: &TrackId) {
        if let Some(saved) = self.saved_tracks.resolved_mut() {
            saved.set.remove(track_id);
            saved.tracks.retain(|t| &t.id != track_id);
        }
    }

    pub fn contains_track(&self, track: &Track) -> bool {
        if let Some(saved) = self.saved_tracks.resolved() {
            saved.set.contains(&track.id)
        } else {
            false
        }
    }

    pub fn add_album(&mut self, album: Arc<Album>) {
        if let Some(saved) = self.saved_albums.resolved_mut() {
            saved.set.insert(album.id.clone());
            saved.albums.push_front(album);
        }
    }

    pub fn remove_album(&mut self, album_id: &str) {
        if let Some(saved) = self.saved_albums.resolved_mut() {
            saved.set.remove(album_id);
            saved.albums.retain(|a| a.id.as_ref() != album_id);
        }
    }

    pub fn contains_album(&self, album: &Album) -> bool {
        if let Some(saved) = self.saved_albums.resolved() {
            saved.set.contains(&album.id)
        } else {
            false
        }
    }

    pub fn add_show(&mut self, show: Arc<Show>) {
        if let Some(saved) = self.saved_shows.resolved_mut() {
            saved.set.insert(show.id.clone());
            saved.shows.push_front(show);
        }
    }

    pub fn remove_show(&mut self, show_id: &str) {
        if let Some(saved) = self.saved_shows.resolved_mut() {
            saved.set.remove(show_id);
            saved.shows.retain(|a| a.id.as_ref() != show_id);
        }
    }

    pub fn contains_show(&self, show: &Show) -> bool {
        if let Some(saved) = self.saved_shows.resolved() {
            saved.set.contains(&show.id)
        } else {
            false
        }
    }

    pub fn writable_playlists(&self) -> Vec<&Playlist> {
        if let Some(saved) = self.playlists.resolved() {
            saved
                .iter()
                .filter(|playlist| {
                    self.user_profile
                        .resolved()
                        .map(|user| playlist.owner.id == user.id)
                        .unwrap_or(false)
                        || playlist.collaborative
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn add_playlist(&mut self, playlist: Playlist) {
        if let Some(playlists) = self.playlists.resolved_mut() {
            playlists.push_back(playlist);
        }
    }

    pub fn remove_from_playlist(&mut self, id: &str) {
        if let Some(playlists) = self.playlists.resolved_mut() {
            playlists.retain(|p| p.id.as_ref() != id);
        }
    }

    pub fn rename_playlist(&mut self, link: PlaylistLink) {
        if let Some(saved) = self.playlists.resolved_mut() {
            for playlist in saved.iter_mut() {
                if playlist.id == link.id {
                    playlist.name = link.name;
                    break;
                }
            }
        }
    }

    pub fn is_created_by_user(&self, playlist: &Playlist) -> bool {
        if let Some(profile) = self.user_profile.resolved() {
            profile.id == playlist.owner.id
        } else {
            false
        }
    }

    pub fn contains_playlist(&self, playlist: &Playlist) -> bool {
        if let Some(playlists) = self.playlists.resolved() {
            playlists.iter().any(|p| p.id == playlist.id)
        } else {
            false
        }
    }

    pub fn increment_playlist_track_count(&mut self, link: &PlaylistLink) {
        if let Some(saved) = self.playlists.resolved_mut() {
            if let Some(playlist) = saved.iter_mut().find(|p| p.id == link.id) {
                playlist.track_count = playlist.track_count.map(|count| count + 1);
            }
        }
    }

    pub fn decrement_playlist_track_count(&mut self, link: &PlaylistLink) {
        if let Some(saved) = self.playlists.resolved_mut() {
            if let Some(playlist) = saved.iter_mut().find(|p| p.id == link.id) {
                playlist.track_count = playlist.track_count.map(|count| count.saturating_sub(1));
            }
        }
    }
}

impl Default for Library {
    fn default() -> Self {
        Library {
            user_profile: Promise::Empty,
            playlists: Promise::Empty,
            saved_albums: Promise::Empty,
            saved_tracks: Promise::Empty,
            saved_shows: Promise::Empty,
            followed_artists: Promise::Empty,
        }
    }
}

#[derive(Clone, Data, Lens)]
pub struct SavedTracks {
    pub tracks: Vector<Arc<Track>>,
    pub set: HashSet<TrackId>,
    pub loaded_at: Instant,
}

impl SavedTracks {
    pub fn new(tracks: Vector<Arc<Track>>) -> Self {
        let set = tracks.iter().map(|t| t.id).collect();
        Self {
            tracks,
            set,
            loaded_at: Instant::now(),
        }
    }
}

#[derive(Clone, Default, Data, Lens)]
pub struct SavedAlbums {
    pub albums: Vector<Arc<Album>>,
    pub set: HashSet<Arc<str>>,
}

impl SavedAlbums {
    pub fn new(albums: Vector<Arc<Album>>) -> Self {
        let set = albums.iter().map(|a| a.id.clone()).collect();
        Self { albums, set }
    }
}

#[derive(Clone, Default, Data, Lens)]
pub struct Shows {
    pub shows: Vector<Arc<Show>>,
    pub set: HashSet<Arc<str>>,
}

impl Shows {
    pub fn new(shows: Vector<Arc<Show>>) -> Self {
        let set = shows.iter().map(|a| a.id.clone()).collect();
        Self { shows, set }
    }
}

#[derive(Clone, Data)]
pub struct CommonCtx {
    /// The playing item, and the list it plays from.
    pub now_playing: Option<(Playable, PlaybackOrigin)>,
    pub library: Arc<Library>,
    pub show_track_cover: bool,
    pub nav: Nav,
    /// Tracks that play without a connection, and the ones being downloaded.
    pub downloaded: HashSet<TrackId>,
    pub downloading: HashSet<TrackId>,
    /// Playlists downloaded for offline listening: their track count back then,
    /// and their tracks that can be downloaded (local files can't).
    pub offline_playlists: HashMap<Arc<str>, (usize, Vector<TrackId>)>,
}

impl CommonCtx {
    /// Whether `item` of the list `origin` plays: the same track in another
    /// album or playlist doesn't count.
    pub fn is_playing(&self, item: &Playable, origin: &PlaybackOrigin) -> bool {
        matches!(&self.now_playing, Some((playing, from)) if playing.id() == item.id() && from == origin)
    }

    /// Whether the whole playlist plays without a connection.  A changed track
    /// count means tracks were added or removed since it was downloaded.
    pub fn is_offline(&self, playlist: &Playlist) -> bool {
        self.offline_playlists
            .get(&playlist.id)
            .is_some_and(|(count, tracks)| {
                playlist.track_count == Some(*count)
                    && tracks.iter().all(|id| self.downloaded.contains(id))
            })
    }

    // In the cache, so Clear Cache forgets them together with the downloads.
    fn offline_playlists_path() -> Option<PathBuf> {
        Config::cache_dir().map(|dir| dir.join("offline-playlists.json"))
    }

    pub fn load_offline_playlists() -> HashMap<Arc<str>, (usize, Vector<TrackId>)> {
        Self::offline_playlists_path()
            .and_then(|path| fs::read(path).ok())
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default()
    }

    pub fn save_offline_playlists(&self) {
        let Some(path) = Self::offline_playlists_path() else {
            return;
        };
        let result = serde_json::to_vec(&self.offline_playlists)
            .map_err(|err| err.to_string())
            .and_then(|json| utils::write_atomic(&path, &json).map_err(|err| err.to_string()));
        if let Err(err) = result {
            log::warn!("failed to save offline playlists: {err}");
        }
    }
}

pub type WithCtx<T> = Ctx<Arc<CommonCtx>, T>;

#[derive(Clone, Data, Lens)]
pub struct HomeDetail {
    pub made_for_you: Promise<MixedView>,
    pub user_top_mixes: Promise<MixedView>,
    pub best_of_artists: Promise<MixedView>,
    pub recommended_stations: Promise<MixedView>,
    pub uniquely_yours: Promise<MixedView>,
    pub your_shows: Promise<MixedView>,
    pub shows_that_you_might_like: Promise<MixedView>,
    pub jump_back_in: Promise<MixedView>,
    pub user_top_tracks: Promise<Vector<Arc<Track>>>,
    pub user_top_artists: Promise<Vector<Artist>>,
}

#[derive(Clone, Data, Lens)]
pub struct MixedView {
    pub title: Arc<str>,
    pub playlists: Vector<Playlist>,
    pub artists: Vector<Artist>,
    pub albums: Vector<Arc<Album>>,
    pub shows: Vector<Arc<Show>>,
}

static ALERT_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Data, Lens)]
pub struct Alert {
    pub id: usize,
    pub message: Arc<str>,
    pub style: AlertStyle,
    pub created_at: Instant,
}

impl Alert {
    fn fresh_id() -> usize {
        ALERT_ID.fetch_add(1, Ordering::SeqCst)
    }
}

#[derive(Clone, Data, Eq, PartialEq)]
pub enum AlertStyle {
    Error,
    Info,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::local_library::{local_track, LocalItem};
    use psst_core::item_id::ItemIdType;

    fn track(id: u128) -> Arc<Track> {
        let item = LocalItem {
            title: id.to_string(),
            artists: vec![],
            album: String::new(),
            duration_ms: None,
        };
        let mut track = local_track(&item, None, 1, 1);
        track.id = TrackId(ItemId::new(id, ItemIdType::Track));
        Arc::new(track)
    }

    #[test]
    fn each_track_list_keeps_its_own_sort() {
        let mut state = AppState::default_with_config(Config::default());
        let saved = SavedTracks::new([track(1), track(2)].into_iter().collect());
        state.with_library_mut(|library| library.saved_tracks.resolve_or_reject((), Ok(saved)));
        let link = PlaylistLink {
            id: "p".into(),
            name: "P".into(),
        };
        let tracks = PlaylistTracks {
            id: link.id.clone(),
            name: link.name.clone(),
            tracks: [track(1), track(2)].into_iter().collect(),
        };
        state.playlist_detail.tracks.resolve(link.clone(), tracks);
        let by_title = (SortCriteria::Title, SortOrder::Descending);
        state.config.track_sorts.insert("p".into(), by_title);
        state.resort_tracks();

        let titles = |tracks: &Vector<Arc<Track>>| -> Vec<String> {
            tracks.iter().map(|t| t.name.to_string()).collect()
        };
        let playlist = state.playlist_detail.tracks.resolved().unwrap();
        assert_eq!(titles(&playlist.tracks), ["2", "1"]);
        let saved = state.library.saved_tracks.resolved().unwrap();
        assert_eq!(titles(&saved.tracks), ["1", "2"]);

        state.navigate(&Nav::PlaylistDetail(link));
        assert_eq!(state.current_sort(), by_title);
        state.navigate(&Nav::SavedTracks);
        assert_eq!(state.current_sort(), Default::default());
    }

    #[test]
    fn playing_track_shows_only_in_its_own_list() {
        let mut state = AppState::default_with_config(Config::default());
        let link = PlaylistLink {
            id: "p".into(),
            name: "P".into(),
        };
        let playlist = PlaybackOrigin::Playlist(link);
        let playing = track(1);
        let item = Playable::Track(playing.clone());
        state.start_playback(item, playlist.clone(), Duration::ZERO);
        // A reloaded list has other copies of the same track.
        let copy = Playable::Track(Arc::new((*playing).clone()));
        assert!(state.common_ctx.is_playing(&copy, &playlist));
        assert!(!state.common_ctx.is_playing(&copy, &PlaybackOrigin::Library));
        let other = Playable::Track(track(2));
        assert!(!state.common_ctx.is_playing(&other, &playlist));
    }

    #[test]
    fn saved_playback_round_trips_the_queue() {
        let mut state = AppState::default_with_config(Config::default());
        let origin = PlaybackOrigin::Library;
        let mut local = (*track(2)).clone();
        local.local_path = Some(r"C:\Music\b.mp3".into());
        local.id = TrackId(ItemId::from_local(r"C:\Music\b.mp3".into()));
        local.duration = Duration::from_millis(123_456);
        let mut missing = (*track(3)).clone();
        missing.id = TrackId(ItemId::new(0, ItemIdType::LocalFile));
        let tracks = [track(1), Arc::new(local), Arc::new(missing)];
        state.playback.queue = tracks
            .iter()
            .map(|track| QueueEntry {
                item: Playable::Track(track.clone()),
                origin: origin.clone(),
            })
            .collect();
        let playing = Playable::Track(tracks[1].clone());
        state.start_playback(playing, origin.clone(), Duration::from_secs(42));

        let saved = SavedPlayback::of(&state.playback).unwrap();
        assert_eq!((saved.position, saved.progress_ms), (1, 42_000));
        let json = serde_json::to_string(&saved).unwrap();
        let loaded: SavedPlayback = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&loaded).unwrap(), json);
        assert_eq!(loaded.origin, origin);
        let track = &loaded.tracks[1];
        assert_eq!(track.id, tracks[1].id);
        assert_eq!(track.duration, Duration::from_millis(123_456));
        assert_eq!(track.local_path.as_deref(), Some(r"C:\Music\b.mp3"));
        // Local IDs only hold for one run, so they go by path.
        assert!(json.contains(r#""id":"local:C:\\Music\\b.mp3""#));
        assert_eq!(loaded.tracks[2].id, tracks[2].id);

        // Nothing playing, nothing to keep.
        state.playback.now_playing = None;
        assert!(SavedPlayback::of(&state.playback).is_none());
    }

    #[test]
    fn download_progress() {
        let mut state = AppState::default_with_config(Config::default());
        let (a, b, c) = (track(1), track(2), track(3));
        state.common_ctx_mut().downloaded.insert(c.id);
        // Duplicates and already downloaded tracks don't count.
        state.queue_downloads(&[a.clone(), b.clone(), a.clone(), c].into_iter().collect());
        assert_eq!((state.download_queue.len(), state.download_total), (2, 2));
        state.finish_download(a.id, true);
        assert_eq!(state.download_queue.front().unwrap().id, b.id);
        assert_eq!(state.download_total, 2);
        state.finish_download(b.id, false);
        assert_eq!((state.download_queue.len(), state.download_total), (0, 0));
        assert!(state.common_ctx.downloaded.contains(&a.id));
        assert!(!state.common_ctx.downloaded.contains(&b.id));
        assert!(state.common_ctx.downloading.is_empty());
    }

    #[test]
    fn add_tracks_puts_new_ones_on_top() {
        let mut state = AppState::default_with_config(Config::default());
        let saved = SavedTracks::new([track(1)].into_iter().collect());
        state.with_library_mut(|library| library.saved_tracks.resolve_or_reject((), Ok(saved)));
        let mut added = 0;
        state.with_library_mut(|library| {
            added = library.add_tracks([track(2), track(3), track(1), track(3)]);
        });
        assert_eq!(added, 2);
        let saved = state.library.saved_tracks.resolved().unwrap();
        let order: Vec<_> = saved.tracks.iter().map(|t| t.id).collect();
        assert_eq!(order, [track(2).id, track(3).id, track(1).id]);
        assert_eq!(saved.set.len(), 3);
    }

    #[test]
    fn artist_saved_tracks_follow_the_library() {
        let artist = |id: &str| ArtistLink {
            id: id.into(),
            name: id.to_uppercase().into(),
        };
        let by = |id: u128, artists: &[&str]| {
            let mut t = track(id);
            Arc::make_mut(&mut t).artists = artists.iter().map(|a| artist(a)).collect();
            t
        };
        let mut state = AppState::default_with_config(Config::default());
        let tracks = [by(1, &["b"]), by(2, &["a", "b"]), by(3, &["a"])];
        let saved = SavedTracks::new(tracks.into_iter().collect());

        state.nav = Nav::ArtistSavedTracks(artist("a"));
        state.with_library_mut(|library| library.saved_tracks.resolve_or_reject((), Ok(saved)));
        let ids: Vec<_> = state
            .artist_detail
            .saved_tracks
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, [by(2, &[]).id, by(3, &[]).id]);

        state.with_library_mut(|library| library.remove_track(&by(3, &[]).id));
        assert_eq!(state.artist_detail.saved_tracks.len(), 1);
    }

    #[test]
    fn offline_playlist() {
        let playlist: Playlist = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "P", "description": "", "tracks": {"total": 2},
            "owner": {"id": "u", "display_name": "U"}, "collaborative": false, "public": null
        }))
        .unwrap();
        let mut state = AppState::default_with_config(Config::default());
        let (a, b) = (track(1).id, track(2).id);
        let common = state.common_ctx_mut();
        common
            .offline_playlists
            .insert(playlist.id.clone(), (2, [a, b].into_iter().collect()));
        common.downloaded.insert(a);
        assert!(!common.is_offline(&playlist));
        common.downloaded.insert(b);
        assert!(common.is_offline(&playlist));
        // A track was added since the download.
        common
            .offline_playlists
            .insert(playlist.id.clone(), (1, [a, b].into_iter().collect()));
        assert!(!common.is_offline(&playlist));
    }
}
