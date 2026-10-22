pub mod file;
pub mod item;
pub mod queue;
mod storage;
mod worker;

use std::{
    mem,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    thread::JoinHandle,
    time::Duration,
};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::{
    audio::output::{AudioOutput, AudioSink, DefaultAudioOutput, DefaultAudioSink},
    cache::CacheHandle,
    cdn::CdnHandle,
    error::Error,
    item_id::ItemId,
    session::SessionService,
};

use self::{
    file::MediaPath,
    item::{LoadedPlaybackItem, PlaybackItem},
    queue::{Queue, QueueBehavior},
    worker::PlaybackManager,
};

const PREVIOUS_TRACK_THRESHOLD: Duration = Duration::from_secs(3);
const STOP_AFTER_CONSECUTIVE_LOADING_FAILURES: usize = 3;
const RETRY_LOADING_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct PlaybackConfig {
    pub bitrate: usize,
    pub pregain: f32,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            bitrate: 320,
            pregain: 3.0,
        }
    }
}

pub struct Player {
    state: PlayerState,
    preload: PreloadState,
    session: SessionService,
    cdn: CdnHandle,
    cache: CacheHandle,
    config: PlaybackConfig,
    queue: Queue,
    sender: Sender<PlayerEvent>,
    receiver: Receiver<PlayerEvent>,
    audio_output_sink: DefaultAudioSink,
    playback_mgr: PlaybackManager,
    consecutive_loading_failures: usize,
    /// The item that failed to load once and is being retried.
    retried_item: Option<PlaybackItem>,
    /// Where the next track to play starts, paused: the last session, restored.
    start_paused_at: Option<Duration>,
    /// Bumped to stop the downloads running.
    download_generation: Arc<AtomicUsize>,
}

impl Player {
    pub fn new(
        session: SessionService,
        cdn: CdnHandle,
        cache: CacheHandle,
        config: PlaybackConfig,
        audio_output: &DefaultAudioOutput,
    ) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            playback_mgr: PlaybackManager::new(audio_output.sink(), sender.clone()),
            session,
            cdn,
            cache,
            config,
            sender,
            receiver,
            audio_output_sink: audio_output.sink(),
            state: PlayerState::Stopped,
            preload: PreloadState::None,
            queue: Queue::new(),
            consecutive_loading_failures: 0,
            retried_item: None,
            start_paused_at: None,
            download_generation: Arc::default(),
        }
    }

    pub fn sender(&self) -> Sender<PlayerEvent> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> Receiver<PlayerEvent> {
        self.receiver.clone()
    }

    pub fn handle(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Command(cmd) => self.handle_command(cmd),
            PlayerEvent::Loaded { item, result } => self.handle_loaded(item, result),
            PlayerEvent::Preloaded { item, result } => self.handle_preloaded(item, result),
            PlayerEvent::Position { position, path } => self.handle_position(position, path),
            PlayerEvent::EndOfTrack => self.handle_end_of_track(),
            PlayerEvent::Loading { .. }
            | PlayerEvent::Upcoming { .. }
            | PlayerEvent::LoadingFailed { .. }
            | PlayerEvent::Downloaded { .. }
            | PlayerEvent::ItemDownloaded { .. }
            | PlayerEvent::DownloadPaused { .. }
            | PlayerEvent::Playing { .. }
            | PlayerEvent::Pausing { .. }
            | PlayerEvent::Resuming { .. }
            | PlayerEvent::Stopped
            | PlayerEvent::Blocked { .. } => {}
        };
    }

    fn handle_command(&mut self, cmd: PlayerCommand) {
        // Whatever the user asks for meanwhile wins over the restored pause.
        let keeps_pause = matches!(
            cmd,
            PlayerCommand::Preload { .. }
                | PlayerCommand::Download { .. }
                | PlayerCommand::CancelDownloads
                | PlayerCommand::Configure { .. }
                | PlayerCommand::SetQueueBehavior { .. }
                | PlayerCommand::AddToQueue { .. }
                | PlayerCommand::SetVolume { .. }
                | PlayerCommand::Pause
        );
        if !keeps_pause {
            self.start_paused_at = None;
        }
        match cmd {
            PlayerCommand::LoadQueue { items, position } => self.load_queue(items, position),
            PlayerCommand::LoadQueuePaused {
                items,
                position,
                progress,
            } => {
                self.start_paused_at = Some(progress);
                self.load_queue(items, position);
            }
            PlayerCommand::LoadAndPlay { item } => self.load_and_play(item),
            PlayerCommand::Preload { item } => self.preload(item),
            PlayerCommand::Download { items } => self.download(items),
            PlayerCommand::CancelDownloads => {
                self.download_generation.fetch_add(1, Ordering::SeqCst);
            }
            PlayerCommand::Pause => self.pause(),
            PlayerCommand::Resume => self.resume(),
            PlayerCommand::PauseOrResume => self.pause_or_resume(),
            PlayerCommand::Previous => self.previous(),
            PlayerCommand::Next => self.next(),
            PlayerCommand::Stop => self.stop(),
            PlayerCommand::Seek { position } => self.seek(position),
            PlayerCommand::Configure { config } => self.configure(config),
            PlayerCommand::SetQueueBehavior { behavior } => {
                self.queue.set_behaviour(behavior);
                self.report_upcoming();
            }
            PlayerCommand::AddToQueue { item } => {
                self.queue.add(item);
                self.report_upcoming();
            }
            PlayerCommand::SetUpcoming { items } => {
                self.queue.set_upcoming(items);
                self.report_upcoming();
            }
            PlayerCommand::SetVolume { volume } => self.set_volume(volume),
        }
    }

    fn handle_loaded(&mut self, item: PlaybackItem, result: Result<LoadedPlaybackItem, Error>) {
        match self.state {
            PlayerState::Loading {
                item: requested_item,
                ..
            } if item == requested_item => match result {
                Ok(loaded_item) => {
                    self.consecutive_loading_failures = 0;
                    self.retried_item = None;
                    self.play_loaded(loaded_item);
                }
                // Most loading errors are hiccups (audio key or CDN timeouts), give
                // them a second chance before skipping the track.  A missing file
                // won't appear on a retry.
                Err(err)
                    if self.retried_item != Some(item)
                        && !matches!(err, Error::MediaFileNotFound) =>
                {
                    log::warn!("error while loading, retrying: {err}");
                    self.retried_item = Some(item);
                    self.state = PlayerState::Loading {
                        item,
                        _loading_handle: self.spawn_load(item, RETRY_LOADING_AFTER),
                    };
                }
                Err(err) => {
                    self.retried_item = None;
                    self.consecutive_loading_failures += 1;
                    let _ = self.sender.send(PlayerEvent::LoadingFailed {
                        item,
                        error: err.to_string(),
                    });
                    if self.consecutive_loading_failures < STOP_AFTER_CONSECUTIVE_LOADING_FAILURES {
                        log::error!("skipping, error while loading: {err}");
                        self.next();
                    } else {
                        log::error!("stopping, error while loading: {err}");
                        self.stop();
                    }
                }
            },
            _ => {
                log::info!("stale load result received, ignoring");
            }
        }
    }

    fn handle_preloaded(&mut self, item: PlaybackItem, result: Result<LoadedPlaybackItem, Error>) {
        match self.preload {
            PreloadState::Preloading {
                item: requested_item,
                ..
            } if item == requested_item => match result {
                Ok(loaded_item) => {
                    log::info!("preloaded audio file");
                    self.preload = PreloadState::Preloaded {
                        item,
                        loaded_item: Box::new(loaded_item),
                    };
                }
                Err(err) => {
                    log::error!("failed to preload audio file, error while opening: {err}");
                    self.preload = PreloadState::None;
                }
            },
            _ => {
                log::info!("stale preload result received, ignoring");

                // We are not preloading this item, but because we sometimes extract the
                // preloading thread and use it for loading, let's check if the item is not
                // being loaded now.
                self.handle_loaded(item, result);
            }
        }
    }

    fn handle_position(&mut self, new_position: Duration, path: MediaPath) {
        match &mut self.state {
            PlayerState::Playing { position, .. } | PlayerState::Paused { position, .. } => {
                *position = new_position;
            }
            _ => {
                log::warn!("received unexpected position report");
            }
        }
        const PRELOAD_BEFORE_END_OF_TRACK: Duration = Duration::from_secs(30);
        let time_until_end_of_track = path.duration.checked_sub(new_position).unwrap_or_default();
        if time_until_end_of_track <= PRELOAD_BEFORE_END_OF_TRACK {
            if let Some(&item_to_preload) = self.queue.get_following() {
                self.preload(item_to_preload);
            }
        }
    }

    fn handle_end_of_track(&mut self) {
        if let PlayerState::Playing { path, position } = &self.state {
            // A track "ending" right away usually means its audio stream broke,
            // which looks like a skip.  Leave a trace for diagnosing it.
            if *position < Duration::from_secs(2) && path.duration > Duration::from_secs(10) {
                log::warn!(
                    "track {:?} ended after {position:?} of {:?}, its audio stream probably failed",
                    path.item_id,
                    path.duration
                );
            }
        }
        self.queue.skip_to_following();
        if let Some(&item) = self.queue.get_current() {
            self.load_and_play(item);
        } else {
            self.stop();
        }
    }

    fn load_queue(&mut self, items: Vec<PlaybackItem>, position: usize) {
        self.queue.fill(items, position);
        if let Some(&item) = self.queue.get_current() {
            self.load_and_play(item);
        } else {
            self.stop();
        }
    }

    /// Tells what plays next, for showing the queue.
    fn report_upcoming(&self) {
        let upcoming = self
            .queue
            .upcoming()
            .iter()
            .map(|item| item.item_id)
            .collect();
        let _ = self.sender.send(PlayerEvent::Upcoming { items: upcoming });
    }

    fn load_and_play(&mut self, item: PlaybackItem) {
        // Make sure to stop the sink, so any current audio source is cleared and the
        // playback stopped.
        self.audio_output_sink.stop();
        self.report_upcoming();

        // Check if the item is already in the preloader state.
        let loading_handle = match mem::replace(&mut self.preload, PreloadState::None) {
            PreloadState::Preloaded {
                item: preloaded_item,
                loaded_item,
            } if preloaded_item == item => {
                // This item is already loaded in the preloader state.
                self.play_loaded(*loaded_item);
                return;
            }

            PreloadState::Preloading {
                item: preloaded_item,
                loading_handle,
            } if preloaded_item == item => {
                // This item is being preloaded. Take it out of the preloader state.
                loading_handle
            }

            preloading_other_file_or_none => {
                self.preload = preloading_other_file_or_none;
                // Item is not preloaded yet, load it in a background thread.
                self.spawn_load(item, Duration::ZERO)
            }
        };

        self.sender.send(PlayerEvent::Loading { item }).unwrap();
        self.state = PlayerState::Loading {
            item,
            _loading_handle: loading_handle,
        };
    }

    /// Loads `item` in a background thread, after `delay`, and reports back
    /// with `PlayerEvent::Loaded`.
    fn spawn_load(&self, item: PlaybackItem, delay: Duration) -> JoinHandle<()> {
        let sender = self.sender.clone();
        let session = self.session.clone();
        let cdn = self.cdn.clone();
        let cache = self.cache.clone();
        let config = self.config.clone();
        thread::spawn(move || {
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            let result = item.load(&session, cdn, cache, &config);
            let _ = sender.send(PlayerEvent::Loaded { item, result });
        })
    }

    /// Downloads `items` into the cache one by one in a background thread, see
    /// `download_items`.
    fn download(&self, items: Vec<PlaybackItem>) {
        let sender = self.sender.clone();
        let session = self.session.clone();
        let cdn = self.cdn.clone();
        let cache = self.cache.clone();
        let config = self.config.clone();
        let generation = Arc::clone(&self.download_generation);
        let started = generation.load(Ordering::SeqCst);
        thread::spawn(move || {
            download_items(
                &items,
                |item| item.download(&session, &cdn, &cache, &config),
                thread::sleep,
                |event| {
                    let _ = sender.send(event);
                },
                || generation.load(Ordering::SeqCst) != started,
            );
        });
    }

    fn preload(&mut self, item: PlaybackItem) {
        if self.is_in_preload(item) {
            return;
        }
        let loading_handle = thread::spawn({
            let sender = self.sender.clone();
            let session = self.session.clone();
            let cdn = self.cdn.clone();
            let cache = self.cache.clone();
            let config = self.config.clone();
            move || {
                let result = item.load(&session, cdn, cache, &config);
                sender
                    .send(PlayerEvent::Preloaded { item, result })
                    .unwrap();
            }
        });
        self.preload = PreloadState::Preloading {
            item,
            loading_handle,
        };
    }

    fn set_volume(&mut self, volume: f64) {
        self.audio_output_sink.set_volume(volume as f32);
    }

    fn play_loaded(&mut self, loaded_item: LoadedPlaybackItem) {
        log::info!("starting playback");
        let path = loaded_item.file.path();
        let position = Duration::default();
        self.playback_mgr.play(loaded_item);
        self.state = PlayerState::Playing { path, position };
        self.sender
            .send(PlayerEvent::Playing { path, position })
            .unwrap();
        if let Some(progress) = self.start_paused_at.take() {
            self.pause();
            self.seek(progress);
        }
    }

    fn pause(&mut self) {
        match mem::replace(&mut self.state, PlayerState::Invalid) {
            PlayerState::Playing { path, position } | PlayerState::Paused { path, position } => {
                log::info!("pausing playback");
                self.audio_output_sink.pause();
                self.sender
                    .send(PlayerEvent::Pausing { path, position })
                    .unwrap();
                self.state = PlayerState::Paused { path, position };
            }
            _ => {
                log::warn!("invalid state transition");
            }
        }
    }

    fn resume(&mut self) {
        match mem::replace(&mut self.state, PlayerState::Invalid) {
            PlayerState::Playing { path, position } | PlayerState::Paused { path, position } => {
                log::info!("resuming playback");
                self.audio_output_sink.resume();
                self.sender
                    .send(PlayerEvent::Resuming { path, position })
                    .unwrap();
                self.state = PlayerState::Playing { path, position };
            }
            _ => {
                log::warn!("invalid state transition");
            }
        }
    }

    fn pause_or_resume(&mut self) {
        match &self.state {
            PlayerState::Playing { .. } => self.pause(),
            PlayerState::Paused { .. } => self.resume(),
            _ => {
                // Do nothing.
            }
        }
    }

    fn previous(&mut self) {
        if self.is_near_playback_start() {
            self.queue.skip_to_previous();
            if let Some(&item) = self.queue.get_current() {
                self.load_and_play(item);
            } else {
                self.stop();
            }
        } else {
            self.seek(Duration::default());
        }
    }

    fn next(&mut self) {
        self.queue.skip_to_next();
        if let Some(&item) = self.queue.get_current() {
            self.load_and_play(item);
        } else {
            self.stop();
        }
    }

    fn stop(&mut self) {
        self.sender.send(PlayerEvent::Stopped).unwrap();
        self.audio_output_sink.stop();
        self.state = PlayerState::Stopped;
        self.queue.clear();
        self.report_upcoming();
        self.consecutive_loading_failures = 0;
    }

    fn seek(&mut self, position: Duration) {
        self.playback_mgr.seek(position);
    }

    fn configure(&mut self, config: PlaybackConfig) {
        self.config = config;
    }

    fn is_near_playback_start(&self) -> bool {
        match self.state {
            PlayerState::Playing { position, .. } | PlayerState::Paused { position, .. } => {
                position < PREVIOUS_TRACK_THRESHOLD
            }
            _ => false,
        }
    }

    fn is_in_preload(&self, item: PlaybackItem) -> bool {
        match self.preload {
            PreloadState::Preloading { item: p_item, .. }
            | PreloadState::Preloaded { item: p_item, .. } => p_item == item,
            _ => false,
        }
    }
}

/// Downloads `items` one by one with `download`, retrying failures (usually
/// dropped connections) and waiting out audio key refusals with `sleep`.  Reports
/// each item and each wait with `send`, and then `PlayerEvent::Downloaded`.
fn download_items(
    items: &[PlaybackItem],
    mut download: impl FnMut(&PlaybackItem) -> Result<(), Error>,
    mut sleep: impl FnMut(Duration),
    mut send: impl FnMut(PlayerEvent),
    cancelled: impl Fn() -> bool,
) {
    const ATTEMPTS: usize = 3;
    const RETRY_AFTER: Duration = Duration::from_secs(5);
    // After a few dozen tracks Spotify refuses audio keys for minutes, so wait
    // it out.  The waits restart after each success; once they run out, the
    // rest of the items fail right away.
    const KEY_WAITS_SECS: [u64; 6] = [60, 120, 240, 300, 300, 300];

    let mut failed = 0;
    let mut key_waits = KEY_WAITS_SECS.iter();
    for item in items {
        let mut attempt = 1;
        let ok = loop {
            // Checked after each wait too, they last minutes.
            if cancelled() {
                log::info!("downloads cancelled");
                return;
            }
            match download(item) {
                Ok(()) => {
                    key_waits = KEY_WAITS_SECS.iter();
                    break true;
                }
                Err(Error::AudioKeyRefused) => {
                    let Some(&secs) = key_waits.next() else {
                        failed += 1;
                        break false;
                    };
                    log::warn!("audio key refused, resuming downloads in {secs}s");
                    send(PlayerEvent::DownloadPaused {
                        resume_in: Duration::from_secs(secs),
                    });
                    sleep(Duration::from_secs(secs));
                }
                Err(err) if attempt == ATTEMPTS || matches!(err, Error::MediaFileNotFound) => {
                    log::error!("failed to download {:?}: {err}", item.item_id);
                    failed += 1;
                    break false;
                }
                Err(err) => {
                    log::warn!("download failed, retrying: {err}");
                    attempt += 1;
                    sleep(RETRY_AFTER);
                }
            }
        };
        send(PlayerEvent::ItemDownloaded {
            item_id: item.item_id,
            ok,
        });
    }
    send(PlayerEvent::Downloaded {
        downloaded: items.len() - failed,
        failed,
    });
}

pub enum PlayerCommand {
    LoadQueue {
        items: Vec<PlaybackItem>,
        position: usize,
    },
    /// Like `LoadQueue`, but paused at `progress` once loaded: picking up where
    /// the last session left off.
    LoadQueuePaused {
        items: Vec<PlaybackItem>,
        position: usize,
        progress: Duration,
    },
    LoadAndPlay {
        item: PlaybackItem,
    },
    Preload {
        item: PlaybackItem,
    },
    /// Save items into the cache, so they play without a connection.
    Download {
        items: Vec<PlaybackItem>,
    },
    /// Stops the downloads after the item each is on, without more events.
    CancelDownloads,
    Pause,
    Resume,
    PauseOrResume,
    Previous,
    Next,
    Stop,
    Seek {
        position: Duration,
    },
    Configure {
        config: PlaybackConfig,
    },
    SetQueueBehavior {
        behavior: QueueBehavior,
    },
    AddToQueue {
        item: PlaybackItem,
    },
    /// Replaces what plays after the current track, as edited in the queue.
    SetUpcoming {
        items: Vec<PlaybackItem>,
    },
    /// Change playback volume to a value in 0.0..=1.0 range.
    SetVolume {
        volume: f64,
    },
}

pub enum PlayerEvent {
    Command(PlayerCommand),
    /// Track has started loading.  `Loaded` follows.
    Loading {
        item: PlaybackItem,
    },
    /// Track loading either succeeded or failed.  `Playing` follows in case of
    /// success.
    Loaded {
        item: PlaybackItem,
        result: Result<LoadedPlaybackItem, Error>,
    },
    /// Track couldn't be loaded, even after a retry, and gets skipped.
    LoadingFailed {
        item: PlaybackItem,
        error: String,
    },
    /// An item of a `Download` command is in the downloads, or failed to
    /// download.
    ItemDownloaded {
        item_id: ItemId,
        ok: bool,
    },
    /// Spotify refused an audio key, downloads continue after a wait.
    DownloadPaused {
        resume_in: Duration,
    },
    /// A `Download` command has finished.
    Downloaded {
        downloaded: usize,
        failed: usize,
    },
    /// Next item in queue has been either successfully preloaded or failed to
    /// preload.
    Preloaded {
        item: PlaybackItem,
        result: Result<LoadedPlaybackItem, Error>,
    },
    /// Player has started playing new track.  `Position` events will follow.
    Playing {
        path: MediaPath,
        position: Duration,
    },
    /// Player is in a paused state.  `Resuming` might follow.
    Pausing {
        path: MediaPath,
        position: Duration,
    },
    /// Player is resuming playback of a track.  `Position` events will follow.
    Resuming {
        path: MediaPath,
        position: Duration,
    },
    /// Position of the playback head has changed.
    Position {
        path: MediaPath,
        position: Duration,
    },
    /// Player would like to continue playing, but is blocked, waiting for I/O.
    Blocked {
        path: MediaPath,
        position: Duration,
    },
    /// Player has finished playing a track.  `Loading` or `Playing` might
    /// follow if the queue is not empty, `Stopped` will follow if it is.
    EndOfTrack,
    /// The queue is empty.
    Stopped,
    /// What plays after the current track changed (or the track did).
    Upcoming {
        items: Vec<ItemId>,
    },
}

enum PlayerState {
    Loading {
        item: PlaybackItem,
        _loading_handle: JoinHandle<()>,
    },
    Playing {
        path: MediaPath,
        position: Duration,
    },
    Paused {
        path: MediaPath,
        position: Duration,
    },
    Stopped,
    Invalid,
}

enum PreloadState {
    Preloading {
        item: PlaybackItem,
        loading_handle: JoinHandle<()>,
    },
    Preloaded {
        item: PlaybackItem,
        // Boxed, it's much bigger than the other variants.
        loaded_item: Box<LoadedPlaybackItem>,
    },
    None,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::{audio::normalize::NormalizationLevel, item_id::ItemIdType};

    #[test]
    fn download_retries_and_waits_out_key_refusals() {
        let items: Vec<_> = (1..=5)
            .map(|id| PlaybackItem {
                item_id: ItemId::new(id, ItemIdType::Track),
                norm_level: NormalizationLevel::None,
            })
            .collect();
        let refused = || Err(Error::AudioKeyRefused);
        let mut results: VecDeque<Result<(), Error>> = [
            // 1: one refusal, then fine.
            refused(),
            Ok(()),
            // 2: two dropped connections, then fine.
            Err(Error::SessionDisconnected),
            Err(Error::SessionDisconnected),
            Ok(()),
        ]
        .into_iter()
        // 3: refused through every wait (they restarted after 2), so it fails.
        .chain((0..7).map(|_| refused()))
        // 4: still refused, no waits left.
        .chain([refused()])
        // 5: gone from Spotify, no point retrying.
        .chain([Err(Error::MediaFileNotFound)])
        .collect();

        let mut sleeps = Vec::new();
        let mut events = Vec::new();
        download_items(
            &items,
            |_| results.pop_front().unwrap(),
            |wait| sleeps.push(wait.as_secs()),
            |event| events.push(event),
            || false,
        );

        assert!(results.is_empty());
        assert_eq!(sleeps, [60, 5, 5, 60, 120, 240, 300, 300, 300]);
        let oks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                PlayerEvent::ItemDownloaded { ok, .. } => Some(*ok),
                _ => None,
            })
            .collect();
        assert_eq!(oks, [true, true, false, false, false]);
        assert!(matches!(
            events.last(),
            Some(PlayerEvent::Downloaded {
                downloaded: 2,
                failed: 3
            })
        ));
    }

    #[test]
    fn cancelled_downloads_stop_without_finishing() {
        let items: Vec<_> = (1..=3)
            .map(|id| PlaybackItem {
                item_id: ItemId::new(id, ItemIdType::Track),
                norm_level: NormalizationLevel::None,
            })
            .collect();
        let downloads = std::cell::Cell::new(0);
        let mut events = Vec::new();
        download_items(
            &items,
            |_| {
                downloads.set(downloads.get() + 1);
                Ok(())
            },
            |_| {},
            |event| events.push(event),
            || downloads.get() == 1,
        );
        assert_eq!(downloads.get(), 1);
        // No `Downloaded`: the app already knows it stopped them.
        assert!(matches!(
            events[..],
            [PlayerEvent::ItemDownloaded { ok: true, .. }]
        ));
    }
}
