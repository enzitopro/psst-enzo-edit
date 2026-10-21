use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::Sender;
use druid::{
    im::Vector,
    widget::{prelude::*, Controller},
    Code, ExtEventSink, InternalLifeCycle, KbKey, WindowHandle,
};
use psst_core::{
    audio::{normalize::NormalizationLevel, output::DefaultAudioOutput},
    cache::Cache,
    cdn::Cdn,
    lastfm::{LastFmClient, Scrobbler},
    player::{
        item::{downloaded_tracks, PlaybackItem},
        PlaybackConfig, Player, PlayerCommand, PlayerEvent,
    },
    session::SessionService,
};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    cmd,
    data::Nav,
    data::{
        AppState, Config, NowPlaying, Playable, Playback, PlaybackOrigin, PlaybackState,
        QueueBehavior, QueueEntry, Track, TrackId,
    },
    discord::{DiscordPresence, Presence},
    error::Error,
    ui::lyrics,
    webapi::WebApi,
};

pub struct PlaybackController {
    sender: Option<Sender<PlayerEvent>>,
    thread: Option<JoinHandle<()>>,
    output: Option<DefaultAudioOutput>,
    media_controls: Option<MediaControls>,
    has_scrobbled: bool,
    scrobbler: Option<Scrobbler>,
    discord: DiscordPresence,
    startup: bool,
}
fn init_scrobbler_instance(data: &AppState) -> Option<Scrobbler> {
    if data.config.lastfm_enable {
        if let (Some(api_key), Some(api_secret), Some(session_key)) = (
            data.config.lastfm_api_key.as_deref(),
            data.config.lastfm_api_secret.as_deref(),
            data.config.lastfm_session_key.as_deref(),
        ) {
            match LastFmClient::create_scrobbler(Some(api_key), Some(api_secret), Some(session_key))
            {
                Ok(scr) => {
                    log::info!("Last.fm Scrobbler instance created/updated.");
                    return Some(scr);
                }
                Err(e) => {
                    log::warn!("Failed to create/update Last.fm Scrobbler instance: {e}");
                }
            }
        } else {
            log::info!("Last.fm credentials incomplete or removed, clearing Scrobbler instance.");
        }
    } else {
        log::info!("Last.fm scrobbling is disabled, clearing Scrobbler instance.");
    }
    None
}

impl PlaybackController {
    pub fn new() -> Self {
        Self {
            sender: None,
            thread: None,
            output: None,
            media_controls: None,
            has_scrobbled: false,
            scrobbler: None,
            discord: DiscordPresence::new(),
            startup: true,
        }
    }

    fn open_audio_output_and_start_threads(
        &mut self,
        session: SessionService,
        config: PlaybackConfig,
        event_sink: ExtEventSink,
        widget_id: WidgetId,
        #[allow(unused_variables)] window: &WindowHandle,
    ) {
        let output = DefaultAudioOutput::open().unwrap();
        let cache_dir = Config::cache_dir().unwrap();
        let proxy_url = Config::proxy();
        let cache = Cache::new(cache_dir).unwrap();
        thread::spawn({
            let (cache, config, event_sink) = (cache.clone(), config.clone(), event_sink.clone());
            move || {
                let tracks: Vec<_> = downloaded_tracks(&cache, &config)
                    .into_iter()
                    .map(TrackId)
                    .collect();
                let _ = event_sink.submit_command(cmd::DOWNLOADED_TRACKS, tracks, widget_id);
            }
        });
        let player = Player::new(
            session.clone(),
            Cdn::new(session, proxy_url.as_deref()).unwrap(),
            cache,
            config,
            &output,
        );

        self.media_controls = Self::create_media_controls(player.sender(), window)
            .map_err(|err| log::error!("failed to connect to media control interface: {err:?}"))
            .ok();

        self.sender = Some(player.sender());
        self.thread = Some(thread::spawn(move || {
            Self::service_events(player, event_sink, widget_id);
        }));
        self.output.replace(output);
    }

    fn service_events(mut player: Player, event_sink: ExtEventSink, widget_id: WidgetId) {
        for event in player.receiver() {
            // Forward events that affect the UI state to the UI thread.
            match &event {
                PlayerEvent::Loading { item } => {
                    event_sink
                        .submit_command(cmd::PLAYBACK_LOADING, item.item_id, widget_id)
                        .unwrap();
                }
                PlayerEvent::Playing { path, position } => {
                    let progress = position.to_owned();
                    event_sink
                        .submit_command(cmd::PLAYBACK_PLAYING, (path.item_id, progress), widget_id)
                        .unwrap();
                }
                PlayerEvent::Pausing { .. } => {
                    event_sink
                        .submit_command(cmd::PLAYBACK_PAUSING, (), widget_id)
                        .unwrap();
                }
                PlayerEvent::Resuming { .. } => {
                    event_sink
                        .submit_command(cmd::PLAYBACK_RESUMING, (), widget_id)
                        .unwrap();
                }
                PlayerEvent::Position { position, .. } => {
                    let progress = position.to_owned();
                    event_sink
                        .submit_command(cmd::PLAYBACK_PROGRESS, progress, widget_id)
                        .unwrap();
                }
                PlayerEvent::Blocked { .. } => {
                    event_sink
                        .submit_command(cmd::PLAYBACK_BLOCKED, (), widget_id)
                        .unwrap();
                }
                PlayerEvent::Stopped => {
                    event_sink
                        .submit_command(cmd::PLAYBACK_STOPPED, (), widget_id)
                        .unwrap();
                }
                PlayerEvent::ItemDownloaded { item_id, ok } => {
                    event_sink
                        .submit_command(cmd::TRACK_DOWNLOADED, (TrackId(*item_id), *ok), widget_id)
                        .unwrap();
                }
                PlayerEvent::DownloadPaused { resume_in } => {
                    event_sink
                        .submit_command(cmd::DOWNLOAD_PAUSED, *resume_in, widget_id)
                        .unwrap();
                }
                PlayerEvent::Downloaded { downloaded, failed } => {
                    event_sink
                        .submit_command(
                            cmd::DOWNLOAD_FINISHED,
                            Ok((*downloaded, *failed)),
                            widget_id,
                        )
                        .unwrap();
                }
                PlayerEvent::LoadingFailed { item, error } => {
                    event_sink
                        .submit_command(
                            cmd::PLAYBACK_SKIPPED,
                            (item.item_id, error.clone()),
                            widget_id,
                        )
                        .unwrap();
                }
                _ => {}
            }

            // Let the player react to its internal events.
            player.handle(event);
        }
    }

    fn create_media_controls(
        sender: Sender<PlayerEvent>,
        #[allow(unused_variables)] window: &WindowHandle,
    ) -> Result<MediaControls, souvlaki::Error> {
        let hwnd = {
            #[cfg(target_os = "windows")]
            {
                use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
                let handle = match window.raw_window_handle() {
                    RawWindowHandle::Win32(h) => h,
                    _ => unreachable!(),
                };
                Some(handle.hwnd)
            }
            #[cfg(not(target_os = "windows"))]
            None
        };

        let mut media_controls = MediaControls::new(PlatformConfig {
            dbus_name: format!("com.jpochyla.psst.{}", random_lowercase_string(8)).as_str(),
            display_name: "Psst",
            hwnd,
        })?;

        media_controls.attach(move |event| {
            Self::handle_media_control_event(event, &sender);
        })?;

        Ok(media_controls)
    }

    fn handle_media_control_event(event: MediaControlEvent, sender: &Sender<PlayerEvent>) {
        let cmd = match event {
            MediaControlEvent::Play => PlayerEvent::Command(PlayerCommand::Resume),
            MediaControlEvent::Pause => PlayerEvent::Command(PlayerCommand::Pause),
            MediaControlEvent::Toggle => PlayerEvent::Command(PlayerCommand::PauseOrResume),
            MediaControlEvent::Next => PlayerEvent::Command(PlayerCommand::Next),
            MediaControlEvent::Previous => PlayerEvent::Command(PlayerCommand::Previous),
            MediaControlEvent::SetPosition(MediaPosition(duration)) => {
                PlayerEvent::Command(PlayerCommand::Seek { position: duration })
            }
            _ => {
                return;
            }
        };
        sender.send(cmd).unwrap();
    }

    fn update_media_control_playback(&mut self, playback: &Playback) {
        if let Some(media_controls) = self.media_controls.as_mut() {
            let progress = playback
                .now_playing
                .as_ref()
                .map(|now_playing| MediaPosition(now_playing.progress));
            media_controls
                .set_playback(match playback.state {
                    PlaybackState::Loading | PlaybackState::Stopped => MediaPlayback::Stopped,
                    PlaybackState::Playing => MediaPlayback::Playing { progress },
                    PlaybackState::Paused => MediaPlayback::Paused { progress },
                })
                .unwrap_or_default();
        }
    }

    fn update_media_control_metadata(&mut self, playback: &Playback) {
        if let Some(media_controls) = self.media_controls.as_mut() {
            let title = playback.now_playing.as_ref().map(|p| p.item.name().clone());
            let album = playback
                .now_playing
                .as_ref()
                .and_then(|p| p.item.track())
                .map(|t| t.album_name());
            let artist = playback
                .now_playing
                .as_ref()
                .and_then(|p| p.item.track())
                .map(|t| t.artist_name());
            let duration = playback.now_playing.as_ref().map(|p| p.item.duration());
            let cover_url = playback
                .now_playing
                .as_ref()
                .and_then(|p| p.cover_image_url(512.0, 512.0));
            media_controls
                .set_metadata(MediaMetadata {
                    title: title.as_deref(),
                    album: album.as_deref(),
                    artist: artist.as_deref(),
                    duration,
                    cover_url,
                })
                .unwrap();
        }
    }

    fn send(&mut self, event: PlayerEvent) {
        if let Some(s) = &self.sender {
            s.send(event)
                .map_err(|e| log::error!("error sending message: {e:?}"))
                .ok();
        }
    }

    // Last.fm calls run in their own thread: on the UI thread a slow or dead
    // connection froze the window while the music kept playing.

    fn report_now_playing(&mut self, playback: &Playback) {
        if let Some(now_playing) = playback.now_playing.as_ref() {
            if let Playable::Track(track) = &now_playing.item {
                if let Some(scrobbler) = self.scrobbler.clone() {
                    let artist = track.artist_name().to_string();
                    let title = track.name.to_string();
                    let album = track.album.as_ref().map(|a| a.name.to_string());
                    thread::spawn(move || {
                        match LastFmClient::now_playing_song(
                            &scrobbler,
                            &artist,
                            &title,
                            album.as_deref(),
                        ) {
                            Ok(()) => {
                                log::info!("reported 'Now Playing' to Last.fm: {artist} - {title}")
                            }
                            Err(e) => log::warn!("failed to report 'Now Playing' to Last.fm: {e}"),
                        }
                    });
                } else {
                    log::debug!("Last.fm not configured, skipping now_playing report.");
                }
            }
        }
    }

    fn report_scrobble(&mut self, playback: &Playback) {
        const ATTEMPTS: u32 = 3;
        const RETRY_AFTER: Duration = Duration::from_secs(10);

        if let Some(now_playing) = playback.now_playing.as_ref() {
            if let Playable::Track(track) = &now_playing.item {
                if now_playing.progress >= track.duration / 2 && !self.has_scrobbled {
                    // Once per play: retries happen in the thread, not on every tick.
                    self.has_scrobbled = true;
                    if let Some(scrobbler) = self.scrobbler.clone() {
                        let artist = track.artist_name().to_string();
                        let title = track.name.to_string();
                        let album = track.album.as_ref().map(|a| a.name.to_string());
                        thread::spawn(move || {
                            for attempt in 1..=ATTEMPTS {
                                match LastFmClient::scrobble_song(
                                    &scrobbler,
                                    &artist,
                                    &title,
                                    album.as_deref(),
                                ) {
                                    Ok(()) => {
                                        log::info!(
                                            "scrobbled track to Last.fm: {artist} - {title}"
                                        );
                                        return;
                                    }
                                    Err(e) => {
                                        log::warn!("failed to scrobble track to Last.fm: {e}");
                                        if attempt < ATTEMPTS {
                                            thread::sleep(RETRY_AFTER);
                                        }
                                    }
                                }
                            }
                        });
                    } else {
                        log::debug!("Last.fm not configured, skipping scrobble.");
                    }
                }
            }
        }
    }

    fn play(&mut self, items: &Vector<QueueEntry>, position: usize) {
        let playback_items = items.iter().map(|queued| PlaybackItem {
            item_id: queued.item.id(),
            norm_level: match queued.origin {
                PlaybackOrigin::Album(_) => NormalizationLevel::Album,
                _ => NormalizationLevel::Track,
            },
        });
        let playback_items_vec: Vec<PlaybackItem> = playback_items.collect();

        // Make sure position is within bounds
        let position = if position >= playback_items_vec.len() {
            0
        } else {
            position
        };

        self.send(PlayerEvent::Command(PlayerCommand::LoadQueue {
            items: playback_items_vec,
            position,
        }));
    }

    fn pause(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::Pause));
    }

    fn resume(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::Resume));
    }

    fn pause_or_resume(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::PauseOrResume));
    }

    fn previous(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::Previous));
    }

    fn next(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::Next));
    }

    fn stop(&mut self) {
        self.send(PlayerEvent::Command(PlayerCommand::Stop));
    }

    fn seek(&mut self, position: Duration) {
        self.send(PlayerEvent::Command(PlayerCommand::Seek { position }));
    }

    fn seek_relative(&mut self, data: &AppState, forward: bool) {
        if let Some(now_playing) = &data.playback.now_playing {
            let seek_duration = Duration::from_secs(data.config.seek_duration as u64);

            // Calculate new position, ensuring it does not exceed duration for forward seeks.
            let seek_position = if forward {
                now_playing.progress + seek_duration
            } else {
                now_playing.progress.saturating_sub(seek_duration)
            }
            .min(now_playing.item.duration());

            self.seek(seek_position);
        }
    }

    fn set_volume(&mut self, volume: f64) {
        self.send(PlayerEvent::Command(PlayerCommand::SetVolume { volume }));
    }

    fn add_to_queue(&mut self, item: &PlaybackItem) {
        self.send(PlayerEvent::Command(PlayerCommand::AddToQueue {
            item: *item,
        }));
    }

    /// Fetches the track list in the background and hands it to the player to
    /// download.  `playlist` is the id and track count of a library playlist, to
    /// remember it as downloaded.
    fn download(
        &self,
        ctx: &mut EventCtx,
        playlist: Option<(Arc<str>, usize)>,
        tracks: impl FnOnce() -> Result<Vector<Arc<Track>>, Error> + Send + 'static,
    ) {
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let sink = ctx.get_external_handle();
        let widget_id = ctx.widget_id();
        thread::spawn(move || match tracks() {
            Ok(tracks) => {
                let tracks: Vector<_> = tracks.into_iter().filter(|t| !t.is_local).collect();
                if let Some((id, count)) = playlist {
                    let ids = tracks.iter().map(|t| t.id).collect();
                    let _ = sink.submit_command(cmd::OFFLINE_PLAYLIST, (id, count, ids), widget_id);
                }
                let items = tracks
                    .iter()
                    .map(|t| PlaybackItem {
                        item_id: t.id.0,
                        norm_level: NormalizationLevel::None,
                    })
                    .collect();
                let _ = sink.submit_command(cmd::DOWNLOADING_TRACKS, tracks, widget_id);
                let _ = sender.send(PlayerEvent::Command(PlayerCommand::Download { items }));
            }
            Err(err) => {
                let _ =
                    sink.submit_command(cmd::DOWNLOAD_FINISHED, Err(err.to_string()), widget_id);
            }
        });
    }

    fn set_queue_behavior(&mut self, behavior: QueueBehavior) {
        self.send(PlayerEvent::Command(PlayerCommand::SetQueueBehavior {
            behavior: match behavior {
                QueueBehavior::Sequential => psst_core::player::queue::QueueBehavior::Sequential,
                QueueBehavior::Random => psst_core::player::queue::QueueBehavior::Random,
                QueueBehavior::LoopTrack => psst_core::player::queue::QueueBehavior::LoopTrack,
                QueueBehavior::LoopAll => psst_core::player::queue::QueueBehavior::LoopAll,
            },
        }));
    }

    fn update_lyrics(&mut self, ctx: &mut EventCtx, data: &AppState, now_playing: &NowPlaying) {
        if matches!(data.nav, Nav::Lyrics) || data.lyrics_panel_open {
            ctx.submit_command(lyrics::SHOW_LYRICS.with(now_playing.clone()));
        }
    }
}

impl<W> Controller<AppState, W> for PlaybackController
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
            Event::Command(cmd) if cmd.is(cmd::SET_FOCUS) => {
                ctx.request_focus();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_LOADING) => {
                let item = cmd.get_unchecked(cmd::PLAYBACK_LOADING);

                if let Some(queued) = data.queued_entry(*item) {
                    data.loading_playback(queued.item, queued.origin);
                    self.update_media_control_playback(&data.playback);
                    self.update_media_control_metadata(&data.playback);
                } else {
                    log::warn!("loaded item not found in playback queue");
                }
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_PLAYING) => {
                let (item, progress) = cmd.get_unchecked(cmd::PLAYBACK_PLAYING);

                // Song has changed, so we reset the has_scrobbled value
                self.has_scrobbled = false;
                self.report_now_playing(&data.playback);

                if let Some(queued) = data.queued_entry(*item) {
                    data.start_playback(queued.item, queued.origin, progress.to_owned());
                    self.update_media_control_playback(&data.playback);
                    self.update_media_control_metadata(&data.playback);
                    if let Some(now_playing) = &data.playback.now_playing {
                        self.update_lyrics(ctx, data, now_playing);
                    }
                } else {
                    log::warn!("played item not found in playback queue");
                }
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_PROGRESS) => {
                let progress = cmd.get_unchecked(cmd::PLAYBACK_PROGRESS);
                data.progress_playback(progress.to_owned());

                self.report_scrobble(&data.playback);
                self.update_media_control_playback(&data.playback);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_PAUSING) => {
                data.pause_playback();
                self.update_media_control_playback(&data.playback);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_RESUMING) => {
                data.resume_playback();
                self.update_media_control_playback(&data.playback);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_BLOCKED) => {
                data.block_playback();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_STOPPED) => {
                data.stop_playback();
                self.update_media_control_playback(&data.playback);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAYBACK_SKIPPED) => {
                let (item, error) = cmd.get_unchecked(cmd::PLAYBACK_SKIPPED);
                let name = data
                    .queued_entry(*item)
                    .map(|queued| queued.item.name().to_string())
                    .unwrap_or_else(|| "a track".to_string());
                data.error_alert(format!("Couldn't play \"{name}\", skipped it: {error}"));
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOAD_PLAYLIST) => {
                let link = cmd.get_unchecked(cmd::DOWNLOAD_PLAYLIST).clone();
                data.info_alert(format!(
                    "Downloading \"{}\" for offline listening…",
                    link.name
                ));
                let track_count = data
                    .library
                    .playlists
                    .resolved()
                    .and_then(|playlists| playlists.iter().find(|p| p.id == link.id))
                    .and_then(|playlist| playlist.track_count);
                let playlist = track_count.map(|count| (link.id.clone(), count));
                self.download(ctx, playlist, move || {
                    WebApi::global().get_all_playlist_tracks(&link.id)
                });
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOAD_TRACKS) => {
                let (name, tracks) = cmd.get_unchecked(cmd::DOWNLOAD_TRACKS).clone();
                data.info_alert(format!("Downloading \"{name}\" for offline listening…"));
                self.download(ctx, None, move || Ok(tracks));
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOAD_ALBUM) => {
                let link = cmd.get_unchecked(cmd::DOWNLOAD_ALBUM).clone();
                data.info_alert(format!(
                    "Downloading \"{}\" for offline listening…",
                    link.name
                ));
                self.download(ctx, None, move || {
                    Ok(WebApi::global().get_album(&link.id)?.data.tracks.clone())
                });
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOADED_TRACKS) => {
                let tracks = cmd.get_unchecked(cmd::DOWNLOADED_TRACKS);
                data.common_ctx_mut()
                    .downloaded
                    .extend(tracks.iter().copied());
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::OFFLINE_PLAYLIST) => {
                let (id, count, tracks) = cmd.get_unchecked(cmd::OFFLINE_PLAYLIST).clone();
                let common = data.common_ctx_mut();
                common.offline_playlists.insert(id, (count, tracks));
                common.save_offline_playlists();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOADS_CLEARED) => {
                data.common_ctx_mut().downloaded.clear();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOADING_TRACKS) => {
                data.queue_downloads(cmd.get_unchecked(cmd::DOWNLOADING_TRACKS));
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::TRACK_DOWNLOADED) => {
                let &(id, ok) = cmd.get_unchecked(cmd::TRACK_DOWNLOADED);
                data.finish_download(id, ok);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOAD_PAUSED) => {
                let minutes = cmd
                    .get_unchecked(cmd::DOWNLOAD_PAUSED)
                    .as_secs()
                    .div_ceil(60);
                data.info_alert(format!(
                    "Spotify is limiting downloads, continuing in {minutes} min"
                ));
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::DOWNLOAD_FINISHED) => {
                match cmd.get_unchecked(cmd::DOWNLOAD_FINISHED) {
                    Ok((downloaded, 0)) => data.info_alert(format!(
                        "Downloaded {downloaded} tracks, they now play without a connection"
                    )),
                    Ok((downloaded, failed)) => data.error_alert(format!(
                        "Downloaded {downloaded} tracks, {failed} failed (download again to retry them)"
                    )),
                    Err(err) => data.error_alert(format!("Couldn't download: {err}")),
                }
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_TRACKS) => {
                let payload = cmd.get_unchecked(cmd::PLAY_TRACKS);
                data.playback.queue = payload
                    .items
                    .iter()
                    .map(|item| QueueEntry {
                        origin: payload.origin.to_owned(),
                        item: item.to_owned(),
                    })
                    .collect();

                self.play(&data.playback.queue, payload.position);
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_PAUSE) => {
                self.pause();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_RESUME) => {
                self.resume();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_PREVIOUS) => {
                self.previous();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_NEXT) => {
                self.next();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_STOP) => {
                self.stop();
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::ADD_TO_QUEUE) => {
                log::info!("adding to queue");
                let (entry, item) = cmd.get_unchecked(cmd::ADD_TO_QUEUE);

                self.add_to_queue(item);
                data.add_queued_entry(entry.clone());
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_QUEUE_BEHAVIOR) => {
                let behavior = cmd.get_unchecked(cmd::PLAY_QUEUE_BEHAVIOR);
                data.set_queue_behavior(behavior.to_owned());
                self.set_queue_behavior(behavior.to_owned());
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::PLAY_SEEK) => {
                if let Some(now_playing) = &data.playback.now_playing {
                    let fraction = cmd.get_unchecked(cmd::PLAY_SEEK);
                    let position = Duration::from_secs_f64(
                        now_playing.item.duration().as_secs_f64() * fraction,
                    );
                    self.seek(position);
                }
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(cmd::SKIP_TO_POSITION) => {
                let location = cmd.get_unchecked(cmd::SKIP_TO_POSITION);
                self.seek(Duration::from_millis(*location));
                ctx.set_handled();
            }
            // Keyboard shortcuts.
            Event::KeyDown(key) if key.code == Code::Space => {
                self.pause_or_resume();
                ctx.set_handled();
            }
            Event::KeyDown(key) if key.code == Code::ArrowRight => {
                if key.mods.shift() {
                    self.next();
                } else {
                    self.seek_relative(data, true);
                }
                ctx.set_handled();
            }
            Event::KeyDown(key) if key.code == Code::ArrowLeft => {
                if key.mods.shift() {
                    self.previous();
                } else {
                    self.seek_relative(data, false);
                }
                ctx.set_handled();
            }
            Event::KeyDown(key) if key.key == KbKey::Character("+".to_string()) => {
                data.playback.volume = (data.playback.volume + 0.1).min(1.0);
                ctx.set_handled();
            }
            Event::KeyDown(key) if key.key == KbKey::Character("-".to_string()) => {
                data.playback.volume = (data.playback.volume - 0.1).max(0.0);
                ctx.set_handled();
            }
            _ => child.event(ctx, event, data, env),
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
        match event {
            LifeCycle::WidgetAdded => {
                self.open_audio_output_and_start_threads(
                    data.session.clone(),
                    data.config.playback(),
                    ctx.get_external_handle(),
                    ctx.widget_id(),
                    ctx.window(),
                );

                // Initialize values loaded from the config.
                self.set_volume(data.playback.volume);
                self.set_queue_behavior(data.playback.queue_behavior);

                // Request focus so we can receive keyboard events.
                ctx.submit_command(cmd::SET_FOCUS.to(ctx.widget_id()));
            }
            LifeCycle::Internal(InternalLifeCycle::RouteFocusChanged { new: None, .. }) => {
                // Druid doesn't have any "ambient focus" concept, so we catch the situation
                // when the focus is being lost and sign up to get focused ourselves.
                ctx.submit_command(cmd::SET_FOCUS.to(ctx.widget_id()));
            }
            _ => {}
        }
        if self.startup {
            self.startup = false;
            self.scrobbler = init_scrobbler_instance(data);
            self.discord
                .configure(data.config.discord_presence_app_id());
        }
        child.lifecycle(ctx, event, data, env);
    }

    fn update(
        &mut self,
        child: &mut W,
        ctx: &mut UpdateCtx,
        old_data: &AppState,
        data: &AppState,
        env: &Env,
    ) {
        if !old_data.playback.volume.same(&data.playback.volume) {
            self.set_volume(data.playback.volume);
        }

        let lastfm_changed = old_data.config.lastfm_api_key != data.config.lastfm_api_key
            || old_data.config.lastfm_api_secret != data.config.lastfm_api_secret
            || old_data.config.lastfm_session_key != data.config.lastfm_session_key
            || old_data.config.lastfm_enable != data.config.lastfm_enable;

        if lastfm_changed {
            self.scrobbler = init_scrobbler_instance(data);
        }

        let discord_app_id = data.config.discord_presence_app_id();
        if old_data.config.discord_presence_app_id() != discord_app_id {
            self.discord.configure(discord_app_id);
            self.discord.update(Presence::from_playback(&data.playback));
        } else if !old_data.playback.same(&data.playback) {
            // Unchanged presences (e.g. progress ticks) are filtered out by
            // the Discord thread.
            self.discord.update(Presence::from_playback(&data.playback));
        }

        child.update(ctx, old_data, data, env);
    }
}

// This uses the current system time to generate a random lowercase string of a given length.
fn random_lowercase_string(len: usize) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut n = now;
    let mut chars = Vec::new();
    while n > 0 && chars.len() < len {
        let c = ((n % 26) as u8 + b'a') as char;
        chars.push(c);
        n /= 26;
    }
    while chars.len() < len {
        chars.push('a');
    }
    chars.into_iter().rev().collect()
}
