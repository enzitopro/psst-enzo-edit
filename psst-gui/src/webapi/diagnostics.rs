//! Manual checks against a real account and real files.  Ignored by default,
//! run with e.g.:
//!
//! ```text
//! set PSST_DIAG_FOLDERS=C:\Users\me\Music
//! set PSST_DIAG_PLAYLIST=<playlist id>   (optional)
//! cargo test -p psst-gui diagnostics -- --ignored --nocapture --test-threads=1
//! ```

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Once,
    time::{Duration, Instant},
};

use psst_core::{
    audio::{decode::AudioDecoder, probe::TrackProbe},
    session::SessionService,
};
use symphonia::core::audio::SampleBuffer;

use super::{
    local_library::{LocalItem, LocalLibrary, AUDIO_EXTENSIONS},
    spotify_local_file_paths, WebApi,
};
use crate::data::{Config, Track};

fn folders() -> Vec<PathBuf> {
    std::env::var("PSST_DIAG_FOLDERS")
        .map(|folders| folders.split(';').map(PathBuf::from).collect())
        .unwrap_or_default()
}

static INIT: Once = Once::new();

fn init(config: &mut Config) {
    INIT.call_once(|| {
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Warn)
            .filter_module("psst_gui::webapi::lrclib", log::LevelFilter::Info)
            .is_test(true)
            .try_init();
        let cache = std::env::temp_dir().join("psst-diagnostics");
        LocalLibrary::new(Some(cache)).install_as_global();
        WebApi::new(None, None, config.paginated_limit).install_as_global();
        let session = SessionService::empty();
        session.update_config(config.session());
        WebApi::global().set_session(session);
        let token = config.get_or_refresh_webapi_token().ok();
        WebApi::global()
            .set_webapi_credentials(config.webapi_client_id_value().map(String::from), token);
    });
}

fn local_item(track: &Track) -> LocalItem {
    LocalItem {
        title: track.name.to_string(),
        artists: track.artists.iter().map(|a| a.name.to_string()).collect(),
        album: track.album_name().to_string(),
        duration_ms: Some(track.duration.as_millis() as u64).filter(|ms| *ms > 0),
    }
}

fn report_playlist(id: &str, name: &str, tracks: &[std::sync::Arc<Track>]) {
    let local: Vec<_> = tracks.iter().filter(|t| t.is_local).collect();
    println!(
        "\nPlaylist {name:?} ({id}): {} tracks, {} local",
        tracks.len(),
        local.len()
    );
    for track in local {
        let matched = LocalLibrary::global().find(&local_item(track));
        match matched {
            Some((file, kind)) => println!(
                "  [OK  {kind:?}] {} — {}  ->  {}",
                track.artist_names(),
                track.name,
                file.path.display()
            ),
            None => println!("  [MISSING] {} — {}", track.artist_names(), track.name),
        }
    }
}

#[test]
#[ignore]
fn diagnostics_local_files_and_library() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);

    // 1. Index.
    let started = Instant::now();
    let extra = config
        .username()
        .and_then(|username| spotify_local_file_paths(username).ok())
        .unwrap_or_default();
    let summary = LocalLibrary::global().rescan(&folders(), &extra);
    println!(
        "Indexed {} files ({} read) in {:?}; {} from the Spotify desktop client",
        summary.files,
        summary.read,
        started.elapsed(),
        extra.len()
    );
    let started = Instant::now();
    let again = LocalLibrary::global().rescan(&folders(), &extra);
    println!(
        "Rescan with cache: {} files, {} re-read, in {:?}",
        again.files,
        again.read,
        started.elapsed()
    );

    // 2. Library.
    let playlists = WebApi::global().get_playlists().expect("library");
    let pinned: Vec<_> = playlists.iter().filter(|p| p.pinned).collect();
    let mixes: Vec<_> = playlists
        .iter()
        .filter(|p| p.owner.id.as_ref() == "spotify")
        .collect();
    println!(
        "\nLibrary: {} playlists, {} pinned, {} made by Spotify",
        playlists.len(),
        pinned.len(),
        mixes.len()
    );
    for p in &pinned {
        println!("  pinned: {}", p.name);
    }
    for p in &mixes {
        println!("  spotify: {}", p.name);
    }
    if let Some(mix) = mixes.first() {
        let detail = WebApi::global().get_playlist(&mix.id);
        let tracks = WebApi::global().get_playlist_tracks(&mix.id);
        println!(
            "  first Spotify playlist {:?}: detail {}, {} tracks",
            mix.name,
            if detail.is_ok() { "ok" } else { "FAILED" },
            tracks.map(|t| t.len().to_string()).unwrap_or_else(|e| format!("FAILED ({e})"))
        );
    }

    // 3. Local items in playlists.
    if let Ok(id) = std::env::var("PSST_DIAG_PLAYLIST") {
        let tracks = WebApi::global().get_playlist_tracks(&id).expect("playlist");
        report_playlist(&id, &id, &tracks.iter().cloned().collect::<Vec<_>>());
    } else {
        // Local files can only be added to your own playlists.
        let me = config.username().unwrap_or_default().to_string();
        for playlist in playlists.iter().filter(|p| p.owner.id.as_ref() == me).take(40) {
            match WebApi::global().get_playlist_tracks(&playlist.id) {
                Ok(tracks) if tracks.iter().any(|t| t.is_local) => report_playlist(
                    &playlist.id,
                    &playlist.name,
                    &tracks.iter().cloned().collect::<Vec<_>>(),
                ),
                Ok(_) => {}
                Err(err) => println!("  {}: FAILED ({err})", playlist.name),
            }
        }
    }
}

/// Looks up lyrics for the local tracks (and a few Spotify ones) of the
/// playlists in `PSST_DIAG_PLAYLISTS` (`;`-separated IDs).
#[test]
#[ignore]
fn diagnostics_lyrics() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);

    let ids = std::env::var("PSST_DIAG_PLAYLISTS").expect("PSST_DIAG_PLAYLISTS=<id>;<id>");
    let (mut found, mut synced, mut missing) = (0, 0, 0);
    for id in ids.split(';') {
        let tracks = WebApi::global().get_playlist_tracks(id).expect("playlist");
        let local = tracks.iter().filter(|t| t.is_local);
        let spotify = tracks.iter().filter(|t| !t.is_local).take(3);
        for track in tracks.iter().filter(|t| !t.is_local).take(3) {
            match WebApi::global().get_lyrics(track.id.0.to_base62()) {
                Ok(lines) => println!("  Spotify lyrics for {:?}: {} lines", track.name, lines.len()),
                Err(err) => println!("  Spotify lyrics for {:?}: FAILED {err}", track.name),
            }
        }
        if std::env::var("PSST_DIAG_SPOTIFY_ONLY").is_ok() {
            for track in tracks.iter().filter(|t| !t.is_local).take(3) {
                let source = WebApi::global().lyrics_for_track(track).map(|l| l.source);
                println!("  lyrics_for_track {:?}: {source:?}", track.name);
            }
            continue;
        }
        for track in local.chain(spotify) {
            let started = Instant::now();
            match WebApi::global().lyrics_for_track(track) {
                Ok(lyrics) => {
                    found += 1;
                    if lyrics.synced {
                        synced += 1;
                    }
                    println!(
                        "  [{:<7}] {:>3} lines {:<6} {:>5}ms  {}{} — {}",
                        lyrics.source,
                        lyrics.lines.len(),
                        if lyrics.synced { "synced" } else { "plain" },
                        started.elapsed().as_millis(),
                        if track.is_local { "(local) " } else { "" },
                        track.artist_names(),
                        track.name
                    );
                }
                Err(err) => {
                    missing += 1;
                    println!(
                        "  [MISSING]  {}{} — {}  ({err})",
                        if track.is_local { "(local) " } else { "" },
                        track.artist_names(),
                        track.name
                    );
                }
            }
            // Go easy on LRCLIB.
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    println!("\nfound {found} ({synced} synced), missing {missing}");
}

/// Opens the albums of a few tracks of `PSST_DIAG_PLAYLIST`, like "Go to
/// Album" does.
#[test]
#[ignore]
fn diagnostics_album_detail() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let id = std::env::var("PSST_DIAG_PLAYLIST").expect("PSST_DIAG_PLAYLIST");
    let tracks = WebApi::global().get_playlist_tracks(&id).expect("playlist");
    for track in tracks.iter().filter(|t| !t.is_local).take(3) {
        let album = track.album.as_ref().expect("album link");
        match WebApi::global().get_album(&album.id) {
            Ok(detail) => println!(
                "album {:?} ({}): ok, {} tracks",
                album.name,
                album.id,
                detail.data.tracks.len()
            ),
            Err(err) => println!("album {:?} ({}): FAILED {err}", album.name, album.id),
        }
        // The pathfinder fallback on its own.
        match WebApi::global().get_pathfinder_album_for_tests(&album.id) {
            Ok(detail) => println!(
                "  pathfinder: ok, {:?} by {}, {} tracks, {} ({:?}), label {:?}",
                detail.name,
                detail
                    .artists
                    .iter()
                    .map(|a| a.name.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                detail.tracks.len(),
                detail.release(),
                detail.album_type,
                detail.label
            ),
            Err(err) => println!("  pathfinder: FAILED {err}"),
        }
    }
}

/// Library and playlist edits, each undone right away: like/unlike a track
/// not in the saved tracks, add/remove it to `PSST_DIAG_PLAYLIST` (it must
/// not be in there), and rename that playlist to its own name.
#[test]
#[ignore]
fn diagnostics_library_edits() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let api = WebApi::global();
    let id = std::env::var("PSST_DIAG_PLAYLIST").expect("PSST_DIAG_PLAYLIST");
    let saved: std::collections::HashSet<_> = api
        .get_saved_tracks()
        .expect("saved tracks")
        .iter()
        .map(|t| t.id)
        .collect();
    let in_playlist: std::collections::HashSet<_> = api
        .get_playlist_tracks(&id)
        .expect("playlist")
        .iter()
        .map(|t| t.id)
        .collect();
    let mix = api
        .get_playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.name.contains("Daily Mix"))
        .expect("a daily mix");
    let track = api
        .get_playlist_tracks(&mix.id)
        .unwrap()
        .into_iter()
        .find(|t| !saved.contains(&t.id) && !in_playlist.contains(&t.id))
        .expect("a track neither saved nor in the playlist");
    let base62 = track.id.0.to_base62();
    let uri = track.id.0.to_uri().unwrap();
    println!("test track: {} — {} ({uri})", track.artist_names(), track.name);

    match api.get_user_profile() {
        Ok(profile) => {
            let playlists = api.get_playlists().unwrap();
            let own = playlists.iter().filter(|p| p.owner.id == profile.id).count();
            let editable = playlists.iter().filter(|p| p.collaborative).count();
            println!(
                "profile id {:?}: owns {own} of {} playlists, {editable} editable",
                profile.id,
                playlists.len()
            );
        }
        Err(err) => println!("profile: FAILED {err}"),
    }
    println!("save track:   {:?}", api.save_track(&base62));
    let saved_now = api.get_saved_tracks().unwrap().iter().any(|t| t.id == track.id);
    println!("  in saved tracks after saving: {saved_now}");
    println!("unsave track: {:?}", api.unsave_track(&base62));
    let saved_now = api.get_saved_tracks().unwrap().iter().any(|t| t.id == track.id);
    println!("  in saved tracks after unsaving: {saved_now}");
    if let Some(album) = &track.album {
        // Only if it wasn't saved before, to leave the library as it was.
        let had_album = api
            .get_saved_albums()
            .unwrap()
            .iter()
            .any(|a| a.id == album.id);
        if !had_album {
            println!("save album:   {:?}", api.save_album(&album.id));
            println!("unsave album: {:?}", api.unsave_album(&album.id));
        }
    }
    println!("add to playlist:      {:?}", api.add_track_to_playlist(&id, &uri));
    println!("remove from playlist: {:?}", api.remove_track_from_playlist(&id, &uri));
    let playlist = api.get_playlist(&id).expect("playlist detail");
    println!(
        "rename to same name:  {:?}",
        api.change_playlist_details(&id, &playlist.name)
    );
}

/// Track menu features: credits.
#[test]
#[ignore]
fn diagnostics_track_menu() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let api = WebApi::global();
    let id = std::env::var("PSST_DIAG_PLAYLIST").expect("PSST_DIAG_PLAYLIST");
    let tracks = api.get_playlist_tracks(&id).unwrap();
    let track = tracks.iter().find(|t| !t.is_local).unwrap();
    match api.get_track_credits(&track.id.0.to_base62()) {
        Ok(_) => println!("credits: ok"),
        Err(err) => println!("credits: FAILED {err}"),
    }
}

/// Loads the home page sections.
#[test]
#[ignore]
fn diagnostics_home_sections() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let api = WebApi::global();
    for (name, result) in [
        ("made for you", api.get_made_for_you()),
        ("top mixes", api.get_top_mixes()),
        ("recommended stations", api.recommended_stations()),
        ("uniquely yours", api.uniquely_yours()),
        ("best of artists", api.best_of_artists()),
        ("jump back in", api.jump_back_in()),
        ("your shows", api.your_shows()),
        ("shows you might like", api.shows_that_you_might_like()),
    ] {
        match result {
            Ok(view) => println!(
                "{name}: ok, {:?}, {} playlists, {} albums, {} artists",
                view.title,
                view.playlists.len(),
                view.albums.len(),
                view.artists.len()
            ),
            Err(err) => println!("{name}: FAILED {err}"),
        }
    }
}

/// Fetches raw album objects from the Web API to see which `album_type`
/// values Spotify sends.
#[test]
#[ignore]
fn diagnostics_album_types() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    for query in ["Yeat", "bladee", "Sewerslvt", "plattmade"] {
        match WebApi::global().raw_search_album_types(query) {
            Ok(types) => println!("{query}: {types:?}"),
            Err(err) => println!("{query}: FAILED {err}"),
        }
    }
}

/// Loads the library playlist named `PSST_DIAG_NAME` and reports what comes
/// back from each source.
#[test]
#[ignore]
fn diagnostics_playlist_by_name() {
    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let name = std::env::var("PSST_DIAG_NAME").expect("PSST_DIAG_NAME");
    let playlists = WebApi::global().get_playlists().expect("library");
    let playlist = playlists
        .iter()
        .find(|p| p.name.as_ref() == name)
        .expect("playlist not in library");
    println!(
        "{:?}: id {}, owner {:?}, description {:?}",
        playlist.name, playlist.id, playlist.owner.id, playlist.description
    );
    match WebApi::global().get_playlist(&playlist.id) {
        Ok(detail) => println!("detail: ok, {:?} tracks", detail.track_count),
        Err(err) => println!("detail: FAILED {err}"),
    }
    match WebApi::global().get_playlist_tracks(&playlist.id) {
        Ok(tracks) => {
            println!("tracks: {}", tracks.len());
            for track in tracks.iter().take(5) {
                println!("  {} — {}", track.artist_names(), track.name);
            }
        }
        Err(err) => println!("tracks: FAILED {err}"),
    }
}

/// Plays the first Spotify track of `PSST_DIAG_PLAYLIST` through the real
/// player, with the saved session, at volume 0.
#[test]
#[ignore]
fn diagnostics_play_spotify_track() {
    use psst_core::{
        audio::{normalize::NormalizationLevel, output::DefaultAudioOutput},
        cache::Cache,
        cdn::Cdn,
        player::{item::PlaybackItem, Player, PlayerCommand, PlayerEvent},
    };

    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let id = std::env::var("PSST_DIAG_PLAYLIST").expect("PSST_DIAG_PLAYLIST");
    let tracks = WebApi::global().get_playlist_tracks(&id).expect("playlist");
    let track = tracks.iter().find(|t| !t.is_local).expect("a Spotify track");
    println!("playing {:?} ({:?})", track.name, track.id.0);

    let output = DefaultAudioOutput::open().expect("audio output");
    let session = SessionService::empty();
    session.update_config(config.session());
    let mut player = Player::new(
        session.clone(),
        Cdn::new(session, None).unwrap(),
        Cache::new(std::env::temp_dir().join("psst-diagnostics-cache")).unwrap(),
        config.playback(),
        &output,
    );
    let sender = player.sender();
    let receiver = player.receiver();
    sender
        .send(PlayerEvent::Command(PlayerCommand::SetVolume { volume: 0.0 }))
        .unwrap();
    sender
        .send(PlayerEvent::Command(PlayerCommand::LoadQueue {
            items: vec![PlaybackItem {
                item_id: track.id.0,
                norm_level: NormalizationLevel::Track,
            }],
            position: 0,
        }))
        .unwrap();
    let started = Instant::now();
    let mut playing = false;
    let mut last_position = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(20) {
        let Ok(event) = receiver.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        match &event {
            PlayerEvent::Loading { .. } => println!("{:>6} ms  loading", started.elapsed().as_millis()),
            PlayerEvent::Playing { .. } => {
                println!("{:>6} ms  playing", started.elapsed().as_millis());
                playing = true;
            }
            PlayerEvent::Position { position, .. } => last_position = *position,
            PlayerEvent::Stopped => println!("{:>6} ms  stopped", started.elapsed().as_millis()),
            _ => {}
        }
        player.handle(event);
        if playing && last_position >= Duration::from_secs(3) {
            break;
        }
    }
    println!("playing: {playing}, position reached: {last_position:?}");
    assert!(playing && last_position >= Duration::from_secs(3));
}

/// Downloads the first two Spotify tracks of `PSST_DIAG_PLAYLIST` into a fresh
/// cache, then loads and decodes them with a session that can't connect.
#[test]
#[ignore]
fn diagnostics_download_for_offline() {
    use psst_core::{
        audio::{normalize::NormalizationLevel, output::DefaultAudioOutput},
        cache::Cache,
        cdn::Cdn,
        player::{item::PlaybackItem, Player, PlayerCommand, PlayerEvent},
    };

    let mut config = Config::load().expect("Psst config not found, log in first");
    init(&mut config);
    let id = std::env::var("PSST_DIAG_PLAYLIST").expect("PSST_DIAG_PLAYLIST");
    let tracks = WebApi::global().get_playlist_tracks(&id).expect("playlist");
    let items: Vec<_> = tracks
        .iter()
        .filter(|t| !t.is_local)
        .take(2)
        .map(|t| PlaybackItem {
            item_id: t.id.0,
            norm_level: NormalizationLevel::None,
        })
        .collect();
    let cache_dir = std::env::temp_dir().join("psst-diagnostics-offline");
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cache = Cache::new(cache_dir).unwrap();

    let output = DefaultAudioOutput::open().expect("audio output");
    let session = SessionService::empty();
    session.update_config(config.session());
    let mut player = Player::new(
        session.clone(),
        Cdn::new(session, None).unwrap(),
        cache.clone(),
        config.playback(),
        &output,
    );
    let (sender, receiver) = (player.sender(), player.receiver());
    let started = Instant::now();
    sender
        .send(PlayerEvent::Command(PlayerCommand::Download { items: items.clone() }))
        .unwrap();
    let mut reported = 0;
    let (downloaded, failed) = loop {
        let event = receiver.recv_timeout(Duration::from_secs(120)).expect("download finished");
        match event {
            PlayerEvent::Downloaded { downloaded, failed } => break (downloaded, failed),
            PlayerEvent::ItemDownloaded { ok, .. } => reported += usize::from(ok),
            event => player.handle(event),
        }
    };
    println!("downloaded {downloaded}, failed {failed} in {:?}", started.elapsed());
    assert_eq!((downloaded, failed, reported), (items.len(), 0, items.len()));

    // The startup scan finds them.
    let found = psst_core::player::item::downloaded_tracks(&cache, &config.playback());
    assert!(items.iter().all(|item| found.contains(&item.item_id)));

    // No session config: any network access fails, so these must come from the cache.
    let offline = SessionService::empty();
    let cdn = Cdn::new(offline.clone(), None).unwrap();
    for item in &items {
        let mut loaded = item
            .load(&offline, cdn.clone(), cache.clone(), &config.playback())
            .expect("plays offline");
        let mut buffer = SampleBuffer::<f32>::new(8 * 1024, loaded.source.signal_spec());
        for _ in 0..50 {
            loaded.source.read_packet(&mut buffer).expect("decodes");
        }
        println!("{:?} plays offline", item.item_id);
    }
}

/// Lets the first of two local files (`PSST_DIAG_QUEUE`) end on its own, near
/// its end, to check that the following one gets preloaded and played.
#[test]
#[ignore]
fn diagnostics_player_preload() {
    use psst_core::{
        audio::{normalize::NormalizationLevel, output::DefaultAudioOutput, probe::TrackProbe},
        cache::Cache,
        cdn::Cdn,
        item_id::ItemId,
        player::{item::PlaybackItem, PlaybackConfig, Player, PlayerCommand, PlayerEvent},
    };

    let files: Vec<PathBuf> = std::env::var("PSST_DIAG_QUEUE")
        .expect("PSST_DIAG_QUEUE=<short file>;<file>")
        .split(';')
        .map(PathBuf::from)
        .collect();
    let duration = TrackProbe::new(&files[0]).unwrap().duration.unwrap();
    let items: Vec<PlaybackItem> = files
        .iter()
        .take(2)
        .map(|path| PlaybackItem {
            item_id: ItemId::from_local(path.clone()),
            norm_level: NormalizationLevel::Track,
        })
        .collect();

    let output = DefaultAudioOutput::open().expect("audio output");
    let session = SessionService::empty();
    let mut player = Player::new(
        session.clone(),
        Cdn::new(session, None).unwrap(),
        Cache::new(std::env::temp_dir().join("psst-diagnostics-cache")).unwrap(),
        PlaybackConfig::default(),
        &output,
    );
    let sender = player.sender();
    let receiver = player.receiver();
    sender
        .send(PlayerEvent::Command(PlayerCommand::SetVolume { volume: 0.0 }))
        .unwrap();
    sender
        .send(PlayerEvent::Command(PlayerCommand::LoadQueue {
            items: items.clone(),
            position: 0,
        }))
        .unwrap();
    let started = Instant::now();
    let mut seeked = false;
    let mut preloaded = false;
    let mut second_playing = None;
    while started.elapsed() < Duration::from_secs(20) && second_playing.is_none() {
        let Ok(event) = receiver.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        match &event {
            PlayerEvent::Playing { path, .. } if path.item_id == items[0].item_id && !seeked => {
                // Jump to 6 seconds before the end.
                seeked = true;
                let position = duration.saturating_sub(Duration::from_secs(6));
                sender
                    .send(PlayerEvent::Command(PlayerCommand::Seek { position }))
                    .unwrap();
            }
            PlayerEvent::Preloaded { item, result } => {
                preloaded = item.item_id == items[1].item_id && result.is_ok();
                println!("{:>6} ms  preloaded next: {preloaded}", started.elapsed().as_millis());
            }
            PlayerEvent::Playing { path, .. } if path.item_id == items[1].item_id => {
                second_playing = Some(started.elapsed());
            }
            _ => {}
        }
        player.handle(event);
    }
    println!("second track started after {second_playing:?}, preloaded: {preloaded}");
    assert!(preloaded && second_playing.is_some());
}

/// Drives the real player (decoder, resampler, audio output at volume 0)
/// through a queue of local files.
#[test]
#[ignore]
fn diagnostics_player_queue() {
    use psst_core::{
        audio::{normalize::NormalizationLevel, output::DefaultAudioOutput},
        cache::Cache,
        cdn::Cdn,
        item_id::ItemId,
        player::{item::PlaybackItem, PlaybackConfig, Player, PlayerCommand, PlayerEvent},
    };

    let mut files: Vec<PathBuf> = std::env::var("PSST_DIAG_QUEUE")
        .expect("PSST_DIAG_QUEUE=<file>;<file>;<file>")
        .split(';')
        .map(PathBuf::from)
        .collect();
    files.truncate(3);
    assert_eq!(files.len(), 3, "need three files");
    let items: Vec<PlaybackItem> = files
        .iter()
        .map(|path| PlaybackItem {
            item_id: ItemId::from_local(path.clone()),
            norm_level: NormalizationLevel::Track,
        })
        .collect();
    let name_of = |id: ItemId| {
        let i = items.iter().position(|item| item.item_id == id).unwrap();
        format!("#{i} {}", files[i].file_name().unwrap().to_string_lossy())
    };

    let output = DefaultAudioOutput::open().expect("audio output");
    let session = SessionService::empty();
    let mut player = Player::new(
        session.clone(),
        Cdn::new(session, None).unwrap(),
        Cache::new(std::env::temp_dir().join("psst-diagnostics-cache")).unwrap(),
        PlaybackConfig::default(),
        &output,
    );
    let sender = player.sender();
    let receiver = player.receiver();

    let script: Vec<(u64, &str, PlayerCommand)> = vec![
        (0, "volume 0", PlayerCommand::SetVolume { volume: 0.0 }),
        (0, "load queue", PlayerCommand::LoadQueue { items: items.clone(), position: 0 }),
        (2000, "seek to 0:20", PlayerCommand::Seek { position: Duration::from_secs(20) }),
        (4000, "next", PlayerCommand::Next),
        (6000, "next", PlayerCommand::Next),
        (7000, "previous", PlayerCommand::Previous),
        (9000, "pause", PlayerCommand::Pause),
        (10000, "resume", PlayerCommand::Resume),
        (12000, "stop", PlayerCommand::Stop),
    ];
    let mut script = script.into_iter().peekable();
    let started = Instant::now();
    let mut playing = Vec::new();
    let mut last_position = Duration::ZERO;
    let mut max_position_after_seek = Duration::ZERO;
    let mut seeked = false;
    let mut stopped = false;
    while started.elapsed() < Duration::from_secs(14) && !stopped {
        while script
            .peek()
            .is_some_and(|(at, _, _)| started.elapsed() >= Duration::from_millis(*at))
        {
            let (at, label, command) = script.next().unwrap();
            if label.starts_with("seek") {
                seeked = true;
            }
            if label == "next" {
                seeked = false;
            }
            println!("{at:>6} ms  -> {label}");
            sender.send(PlayerEvent::Command(command)).unwrap();
        }
        let Ok(event) = receiver.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        match &event {
            PlayerEvent::Playing { path, .. } => {
                println!(
                    "{:>6} ms  <- playing {} ({:?})",
                    started.elapsed().as_millis(),
                    name_of(path.item_id),
                    path.duration
                );
                playing.push(path.item_id);
            }
            PlayerEvent::Position { position, .. } => {
                last_position = *position;
                if seeked {
                    max_position_after_seek = max_position_after_seek.max(*position);
                }
            }
            PlayerEvent::Pausing { .. } => {
                println!("{:>6} ms  <- paused at {last_position:?}", started.elapsed().as_millis())
            }
            PlayerEvent::Resuming { .. } => {
                println!("{:>6} ms  <- resumed", started.elapsed().as_millis())
            }
            PlayerEvent::Stopped => {
                println!("{:>6} ms  <- stopped", started.elapsed().as_millis());
                stopped = true;
            }
            _ => {}
        }
        player.handle(event);
    }

    let order: Vec<String> = playing.iter().map(|id| name_of(*id)).collect();
    println!("played in order: {order:?}");
    println!("position reached after seek: {max_position_after_seek:?}");
    assert_eq!(
        playing,
        vec![items[0].item_id, items[1].item_id, items[2].item_id, items[1].item_id]
    );
    assert!(max_position_after_seek >= Duration::from_secs(20));
    assert!(stopped);
}

/// Copies one file per tag format to a temp folder, writes ReplayGain tags
/// into the copies, and checks the decoder picks them up.
#[test]
#[ignore]
fn diagnostics_replay_gain_tags() {
    use lofty::{
        config::WriteOptions,
        prelude::{ItemKey, TagExt, TaggedFileExt},
        tag::Tag,
    };
    use psst_core::audio::normalize::NormalizationLevel;

    let tmp = std::env::temp_dir().join("psst-diagnostics-replaygain");
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, path) in first_file_per_extension() {
        if !["mp3", "flac", "m4a"].contains(&ext.as_str()) {
            continue;
        }
        let copy = tmp.join(format!("replaygain.{ext}"));
        std::fs::copy(&path, &copy).unwrap();
        let mut tagged = lofty::read_from_path(&copy).unwrap();
        if tagged.primary_tag().is_none() {
            let tag_type = tagged.primary_tag_type();
            tagged.insert_tag(Tag::new(tag_type));
        }
        let tag = tagged.primary_tag_mut().unwrap();
        tag.insert_text(ItemKey::ReplayGainTrackGain, "-7.50 dB".into());
        tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.950000".into());
        tag.insert_text(ItemKey::ReplayGainAlbumGain, "-9.00 dB".into());
        tag.save_to_path(&copy, WriteOptions::default()).unwrap();

        let (_, data) = AudioDecoder::probe(std::fs::File::open(&copy).unwrap(), Some(&ext)).unwrap();
        match data {
            Some(data) => println!(
                "{ext:>5}: ReplayGain read, factor track {:.3}, album {:.3} (pregain 3 dB)",
                data.factor_for_level(NormalizationLevel::Track, 3.0),
                data.factor_for_level(NormalizationLevel::Album, 3.0),
            ),
            None => println!("{ext:>5}: ReplayGain NOT read"),
        }
    }
}

fn first_file_per_extension() -> BTreeMap<String, PathBuf> {
    let mut by_extension: BTreeMap<String, PathBuf> = BTreeMap::new();
    for folder in folders() {
        let mut stack = vec![folder];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext = ext.to_ascii_lowercase();
                    if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                        by_extension.entry(ext).or_insert(path);
                    }
                }
            }
        }
    }
    by_extension
}

#[test]
#[ignore]
fn diagnostics_decode_each_format() {
    let mut by_extension = first_file_per_extension();
    // Extra files given explicitly, e.g. generated test tones.
    if let Ok(files) = std::env::var("PSST_DIAG_FILES") {
        for file in files.split(';').map(PathBuf::from) {
            if let Some(ext) = file.extension().and_then(|e| e.to_str()) {
                by_extension.insert(ext.to_ascii_lowercase(), file);
            }
        }
    }
    assert!(!by_extension.is_empty(), "no audio files found");

    for (ext, path) in by_extension {
        let probe = TrackProbe::new(&path).ok().and_then(|p| p.duration);
        let seek_to = probe.map_or(Duration::from_secs(30), |d| (d / 2).min(Duration::from_secs(30)));
        let file = std::fs::File::open(&path).unwrap();
        match AudioDecoder::probe(file, Some(&ext)) {
            Ok((mut decoder, replay_gain)) => {
                let spec = decoder.signal_spec();
                let mut samples = SampleBuffer::<f32>::new(1 << 16, spec);
                let mut decoded = 0usize;
                let mut peak = 0f32;
                let started = Instant::now();
                // About 3 seconds of audio.
                let wanted = spec.rate as usize * spec.channels.count() * 3;
                while decoded < wanted {
                    if decoder.read_packet(&mut samples).is_none() {
                        break;
                    }
                    decoded += samples.samples().len();
                    peak = samples.samples().iter().fold(peak, |p, s| p.max(s.abs()));
                }
                let seek = decoder.seek(seek_to);
                let after_seek = decoder.read_packet(&mut samples).is_some();
                println!(
                    "{ext:>5}: OK  {} Hz, {} ch, probe duration {:?}, decoded {:.1}s in {:?} (peak {:.2}), \
                     seek {:?} {}, ReplayGain {}  [{}]",
                    spec.rate,
                    spec.channels.count(),
                    probe,
                    decoded as f64 / (spec.rate as f64 * spec.channels.count() as f64),
                    started.elapsed(),
                    peak,
                    seek_to,
                    match (seek, after_seek) {
                        (Ok(_), true) => "ok",
                        (Ok(_), false) => "ok (end of file)",
                        (Err(_), _) => "FAILED",
                    },
                    if replay_gain.is_some() { "yes" } else { "no" },
                    path.display()
                );
            }
            Err(err) => println!("{ext:>5}: FAILED {err}  [{}]", path.display()),
        }
    }
}
