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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
        AudioQuality, Authentication, Config, Preferences, PreferencesTab, SortCriteria, SortOrder,
        Theme,
    },
    ctx::Ctx,
    find::{FindQuery, Finder, MatchFindQuery},
    lyrics::{Lyrics, LyricsCandidate, LyricsSearch},
    nav::{Nav, Route, SpotifyUrl},
    playback::{
        NowPlaying, Playable, PlayableMatcher, Playback, PlaybackOrigin, PlaybackPayload,
        PlaybackState, QueueBehavior, QueueEdit, QueueEntry, SavedPlayback,
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

/// How long alerts show: a confirmation briefly, an error long enough to read.
const INFO_ALERT_DURATION: Duration = Duration::from_millis(1500);
const ERROR_ALERT_DURATION: Duration = Duration::from_secs(5);

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
    /// The user scrolled the lyrics away from the line being sung, to read.
    pub lyrics_scrolled: bool,
    /// The queue next to the page.
    pub queue_panel_open: bool,
    /// The main window is the mini player, on top of other windows.
    pub mini_mode: bool,
    /// The mini player shows the lyrics under its controls.
    pub mini_lyrics_open: bool,
    /// The followed artists' releases.
    pub new_releases: Promise<Vector<Arc<Album>>>,
    /// Your Last.fm top of a period ("7day"...).
    pub stats: Promise<Stats, Arc<str>>,
    pub stats_period: Arc<str>,
    /// The queue shows what played instead of what plays next.
    pub queue_shows_history: bool,
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
            hidden_tracks: Library::load_hidden_tracks(),
        });
        let common_ctx = Arc::new(CommonCtx {
            now_playing: None,
            library: Arc::clone(&library),
            show_track_cover: config.show_track_cover,
            nav: Nav::Home,
            downloaded: HashSet::new(),
            downloading: HashSet::new(),
            offline_playlists: HashMap::new(),
            downloaded_only: false,
            playlists_in_custom_order: config.playlist_order == CUSTOM_ORDER,
            selected: HashMap::new(),
        });
        let playback = Playback {
            state: PlaybackState::Stopped,
            now_playing: None,
            queue_behavior: config.queue_behavior,
            queue: Vector::new(),
            upcoming: Vector::new(),
            history: Vector::new(),
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
            lyrics_scrolled: false,
            queue_panel_open: false,
            mini_mode: false,
            mini_lyrics_open: false,
            new_releases: Promise::Empty,
            stats: Promise::Empty,
            stats_period: "1month".into(),
            queue_shows_history: false,
            credits: None,
            download_queue: Vector::new(),
            download_total: 0,
        }
    }
}

impl AppState {
    pub fn navigate(&mut self, nav: &Nav) {
        if &self.nav != nav {
            // shortcut: older builds sharing the config fail to read a page they
            // don't know, so the new pages aren't kept as the last one; drop
            // once every build reads unknown pages as Home.
            let new_page = matches!(nav, Nav::NewReleases | Nav::Stats);
            // A selection is of the page's list.
            if !self.common_ctx.selected.is_empty() {
                self.common_ctx_mut().selected.clear();
            }
            let previous = mem::replace(&mut self.nav, nav.to_owned());
            self.history.push_back(previous);
            if !new_page {
                self.config.last_route.replace(nav.to_owned());
            }
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
        self.refresh_artist_saved_tracks();
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
            (Nav::ArtistSavedTracks(artist), Some(saved)) => {
                let tracks = saved
                    .tracks
                    .iter()
                    .filter(|track| track.artists.iter().any(|a| a.id == artist.id))
                    .cloned()
                    .collect();
                // Each artist has a sort of its own, like the playlists.
                let (criteria, order) = self.config.track_sort(&artist.id);
                sort_tracks(&tracks, criteria, order)
            }
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
    /// Forgets the downloads still to do, the player stops them.
    pub fn cancel_downloads(&mut self) {
        self.common_ctx_mut().downloading.clear();
        self.download_queue.clear();
        self.download_total = 0;
    }

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
        self.alerts.retain(|alert| {
            let duration = match alert.style {
                AlertStyle::Info => INFO_ALERT_DURATION,
                AlertStyle::Error => ERROR_ALERT_DURATION,
            };
            now.duration_since(alert.created_at) < duration
        });
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
    /// Tracks not to play, until a time (seconds since 1970) or for good.
    pub hidden_tracks: HashMap<TrackId, Option<u64>>,
}

/// The library's own order, which the user arranges.
pub const CUSTOM_ORDER: &str = "Custom Order";

/// Seconds since 1970, as the hidden tracks keep their time.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Library {
    pub fn is_hidden(&self, id: &TrackId) -> bool {
        match self.hidden_tracks.get(id) {
            Some(Some(until)) => unix_now() < *until,
            Some(None) => true,
            None => false,
        }
    }

    // Not in the config: a build without this would drop it when saving.
    fn hidden_tracks_path() -> Option<PathBuf> {
        Config::config_dir().map(|dir| dir.join("hidden-tracks.json"))
    }

    /// The hidden tracks, without the ones whose time is over.
    fn load_hidden_tracks() -> HashMap<TrackId, Option<u64>> {
        let hidden: HashMap<TrackId, Option<u64>> = Self::hidden_tracks_path()
            .and_then(|path| fs::read(path).ok())
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default();
        let now = unix_now();
        hidden
            .into_iter()
            .filter(|(_, until)| until.is_none_or(|until| now < until))
            .collect()
    }

    pub fn save_hidden_tracks(&self) {
        let Some(path) = Self::hidden_tracks_path() else {
            return;
        };
        let result = serde_json::to_vec(&self.hidden_tracks)
            .map_err(|err| err.to_string())
            .and_then(|json| utils::write_atomic(&path, &json).map_err(|err| err.to_string()));
        if let Err(err) = result {
            log::warn!("failed to save hidden tracks: {err}");
        }
    }

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

    pub fn change_playlist_details(&mut self, link: PlaylistLink, description: Arc<str>) {
        if let Some(saved) = self.playlists.resolved_mut() {
            for playlist in saved.iter_mut() {
                if playlist.id == link.id {
                    playlist.name = link.name;
                    playlist.description = description;
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

    pub fn increment_playlist_track_count(&mut self, link: &PlaylistLink, by: usize) {
        if let Some(saved) = self.playlists.resolved_mut() {
            if let Some(playlist) = saved.iter_mut().find(|p| p.id == link.id) {
                playlist.track_count = playlist.track_count.map(|count| count + by);
            }
        }
    }

    pub fn decrement_playlist_track_count(&mut self, link: &PlaylistLink, by: usize) {
        if let Some(saved) = self.playlists.resolved_mut() {
            if let Some(playlist) = saved.iter_mut().find(|p| p.id == link.id) {
                playlist.track_count = playlist.track_count.map(|count| count.saturating_sub(by));
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
            hidden_tracks: HashMap::new(),
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
    /// Playlists show and play only what plays without a connection.
    pub downloaded_only: bool,
    /// The sidebar's playlists are in the library's own order, to rearrange.
    pub playlists_in_custom_order: bool,
    /// Tracks picked with Ctrl and Shift clicks, for the selection bar.
    pub selected: HashMap<TrackId, Arc<Track>>,
}

impl CommonCtx {
    /// The selected tracks, in the order of the list they're from.
    pub fn selection(&self) -> Vec<Arc<Track>> {
        let mut tracks: Vec<_> = self.selected.values().cloned().collect();
        tracks.sort_by_key(|t| (t.track_pos, t.disc_number, t.track_number));
        tracks
    }

    /// A Ctrl+click toggles its track, a Shift+click range only adds.
    pub fn select(&mut self, tracks: &[Arc<Track>], range: bool) {
        for track in tracks {
            if !range && self.selected.remove(&track.id).is_some() {
                continue;
            }
            self.selected.insert(track.id, track.clone());
        }
    }

    /// Whether `track` plays without a connection.
    pub fn plays_offline(&self, track: &Track) -> bool {
        self.downloaded.contains(&track.id) || (track.is_local && !track.is_missing_local_file())
    }

    /// Whether `item` of the list `origin` plays: the same track in another
    /// album or playlist doesn't count.
    pub fn is_playing(&self, item: &Playable, origin: &PlaybackOrigin) -> bool {
        matches!(&self.now_playing, Some((playing, from)) if playing.id() == item.id() && from == origin)
    }

    /// Whether what was downloaded as `key` (a playlist ID, or `offline_key` of
    /// an artist) all plays without a connection, `None` if it never was.  A
    /// known `track_count` must still be the one at the download: a changed one
    /// means tracks were added or removed since.
    pub fn offline_state(&self, key: &str, track_count: Option<usize>) -> Option<bool> {
        let (count, tracks) = self.offline_playlists.get(key)?;
        let complete = track_count.is_none_or(|now| now == *count)
            && tracks.iter().all(|id| self.downloaded.contains(id));
        Some(complete)
    }

    /// Where an artist's downloaded saved songs are kept with the playlists.
    pub fn offline_key(artist: &ArtistLink) -> Arc<str> {
        format!("artist:{}", artist.id).into()
    }

    // Next to the downloads: Clear Cache keeps it, Delete Downloads removes it.
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

/// Your Last.fm scrobbles: their count, and the top artists and tracks.
#[derive(Clone, Data, Lens)]
pub struct Stats {
    pub scrobbles: u64,
    pub artists: Vector<StatEntry>,
    pub tracks: Vector<StatEntry>,
}

#[derive(Clone, Data, Lens)]
pub struct StatEntry {
    pub title: Arc<str>,
    pub subtitle: Arc<str>,
    pub plays: u64,
    /// What to search on Spotify for it.
    pub query: Arc<str>,
}

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
        // 42 of 123 seconds: not scrobbled yet, it will be after the restore.
        assert!(!saved.scrobbled());
        let past_half = SavedPlayback {
            progress_ms: 62_000,
            ..SavedPlayback::of(&state.playback).unwrap()
        };
        assert!(past_half.scrobbled());
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
    fn queue_edits_move_remove_and_skip_ahead() {
        let upcoming: Vector<QueueEntry> = (1..=4)
            .map(|n| QueueEntry {
                item: Playable::Track(track(n)),
                origin: PlaybackOrigin::Library,
            })
            .collect();
        let edited = |edit: QueueEdit| -> Vec<u128> {
            let edited = edit.apply(upcoming.clone());
            edited.iter().map(|entry| entry.item.id().id).collect()
        };
        assert_eq!(edited(QueueEdit::MoveUp(2)), [1, 3, 2, 4]);
        assert_eq!(edited(QueueEdit::MoveDown(0)), [2, 1, 3, 4]);
        assert_eq!(edited(QueueEdit::Remove(1)), [1, 3, 4]);
        assert_eq!(edited(QueueEdit::PlayNow(2)), [3, 4]);
        // Off the ends, nothing changes.
        assert_eq!(edited(QueueEdit::MoveUp(0)), [1, 2, 3, 4]);
        assert_eq!(edited(QueueEdit::MoveDown(3)), [1, 2, 3, 4]);
        assert_eq!(edited(QueueEdit::Remove(9)), [1, 2, 3, 4]);
    }

    #[test]
    fn selection_toggles_and_ranges_add() {
        let mut state = AppState::default_with_config(Config::default());
        let common = state.common_ctx_mut();
        common.select(&[track(1)], false);
        common.select(&[track(1), track(2), track(3)], true);
        assert_eq!(common.selected.len(), 3);
        common.select(&[track(2)], false);
        assert!(!common.selected.contains_key(&track(2).id));
        assert_eq!(common.selected.len(), 2);
        // Acted on in the list's order, whatever the clicks' order.
        let mut late = (*track(9)).clone();
        late.track_pos = 7;
        state.common_ctx_mut().select(&[Arc::new(late)], false);
        let order: Vec<_> = state
            .common_ctx
            .selection()
            .iter()
            .map(|t| t.track_pos)
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] <= pair[1]));
        // Leaving the page drops it.
        state.navigate(&Nav::SavedTracks);
        assert!(state.common_ctx.selected.is_empty());
    }

    #[test]
    fn confirmations_go_quickly_errors_stay() {
        let mut state = AppState::default_with_config(Config::default());
        state.info_alert("Added");
        state.error_alert("Failed");
        for alert in state.alerts.iter_mut() {
            alert.created_at -= Duration::from_secs(2);
        }
        state.cleanup_alerts();
        let left: Vec<_> = state.alerts.iter().map(|a| a.style.clone()).collect();
        assert!(matches!(left[..], [AlertStyle::Error]));
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

        // Stopped halfway, nothing shows as downloading any more.
        let (d, e) = (track(4), track(5));
        state.queue_downloads(&[d.clone(), e].into_iter().collect());
        state.finish_download(d.id, true);
        state.cancel_downloads();
        assert_eq!((state.download_queue.len(), state.download_total), (0, 0));
        assert!(state.common_ctx.downloading.is_empty());
        assert!(state.common_ctx.downloaded.contains(&d.id));
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

        // The artist's own sort, apart from the saved tracks' one.
        let by_title = (SortCriteria::Title, SortOrder::Descending);
        state.config.track_sorts.insert("a".into(), by_title);
        state.resort_tracks();
        let ids: Vec<_> = state
            .artist_detail
            .saved_tracks
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, [by(3, &[]).id, by(2, &[]).id]);

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
        let state = |common: &CommonCtx| common.offline_state(&playlist.id, playlist.track_count);
        common
            .offline_playlists
            .insert(playlist.id.clone(), (2, [a, b].into_iter().collect()));
        common.downloaded.insert(a);
        assert_eq!(state(common), Some(false));
        common.downloaded.insert(b);
        assert_eq!(state(common), Some(true));
        // A track was added since the download.
        common
            .offline_playlists
            .insert(playlist.id.clone(), (1, [a, b].into_iter().collect()));
        assert_eq!(state(common), Some(false));
        assert_eq!(common.offline_state(&playlist.id, Some(2)), Some(false));
        assert_eq!(common.offline_state("never", None), None);
        // Artists have no count to check.
        let artist = ArtistLink {
            id: "x".into(),
            name: "X".into(),
        };
        let key = CommonCtx::offline_key(&artist);
        let tracks = [a].into_iter().collect();
        common.offline_playlists.insert(key.clone(), (1, tracks));
        assert_eq!(common.offline_state(&key, None), Some(true));
    }
}
