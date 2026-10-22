use std::{path::Path, sync::Arc, time::Duration};

use druid::{
    commands,
    widget::{CrossAxisAlignment, Either, Flex, Label, LineBreaking, ViewSwitcher},
    Command, Env, Lens, LensExt, LocalizedString, Menu, MenuItem, Size, TextAlignment, Widget,
    WidgetExt,
};
use psst_core::{audio::normalize::NormalizationLevel, player::item::PlaybackItem};

use crate::{
    cmd,
    data::{
        AppState, Library, Nav, PlaybackOrigin, PlaylistAddTrack, PlaylistRemoveTrack, QueueEntry,
        Track,
    },
    ui::playlist,
    widget::{fill_between::FillBetween, icons, Empty, MyWidgetExt, RemoteImage},
};

use super::{
    library,
    playable::{self, PlayRow},
    theme,
    utils::{self, placeholder_widget},
};

#[derive(Copy, Clone)]
pub struct Display {
    pub number: bool,
    pub title: bool,
    pub artist: bool,
    pub album: bool,
    pub cover: bool,
    pub popularity: bool,
}

impl Display {
    pub fn empty() -> Self {
        Display {
            number: false,
            title: false,
            artist: false,
            album: false,
            cover: false,
            popularity: false,
        }
    }
}

pub fn playable_widget(display: Display) -> impl Widget<PlayRow<Arc<Track>>> {
    let mut main_row = Flex::row();
    let mut major = Flex::row().cross_axis_alignment(CrossAxisAlignment::Center);
    let mut minor = Flex::row();

    if display.number {
        let track_number = Label::<Arc<Track>>::dynamic(|track, _| track.track_number.to_string())
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .with_text_color(theme::PLACEHOLDER_COLOR)
            .with_text_alignment(TextAlignment::Center)
            .center()
            .fix_width(theme::grid(2.0))
            .lens(PlayRow::item);
        major.add_child(track_number);
        major.add_default_spacer();

        // Align the bottom line content.
        minor.add_spacer(theme::grid(2.0));
        minor.add_default_spacer();
    }

    if display.cover {
        let album_cover = rounded_cover_widget(theme::grid(4.0))
            .padding_right(theme::grid(1.0))
            .lens(PlayRow::item);
        main_row.add_child(Either::new(
            |row, _| row.ctx.show_track_cover,
            album_cover,
            Empty,
        ));
    }

    if display.title {
        let local_icon = Either::new(
            |row: &PlayRow<Arc<Track>>, _| row.item.is_local,
            icons::LOCAL_FILE
                .scale(theme::ICON_SIZE_SMALL)
                .padding_right(theme::grid(0.5)),
            Empty,
        );
        major.add_child(local_icon);
        // Filled once the track plays without a connection, outlined while downloading.
        major.add_child(Either::new(
            |row: &PlayRow<Arc<Track>>, _| row.ctx.downloaded.contains(&row.item.id),
            icons::DOWNLOADED
                .scale(theme::ICON_SIZE_SMALL)
                .padding_right(theme::grid(0.5)),
            Either::new(
                |row: &PlayRow<Arc<Track>>, _| row.ctx.downloading.contains(&row.item.id),
                icons::DOWNLOADING
                    .scale(theme::ICON_SIZE_SMALL)
                    .with_color(theme::GREY_400)
                    .padding_right(theme::grid(0.5)),
                Empty,
            ),
        ));
        let track_name = Label::raw()
            .with_font(theme::UI_FONT_MEDIUM)
            .with_line_break_mode(LineBreaking::Clip)
            .lens(PlayRow::item.then(Track::name.in_arc()))
            .padding_right(theme::grid(1.0));
        let is_playing = playable::is_playing_marker_widget().lens(PlayRow::is_playing);
        major.add_flex_child(FillBetween::new(track_name, is_playing), 1.0);
    }

    let mut minor_row = Flex::row();
    if display.artist || display.album {
        // Decided per row data, rows get reused for other tracks on sorting.
        minor_row.add_child(Either::new(
            |row: &PlayRow<Arc<Track>>, _| row.item.explicit,
            icons::EXPLICIT
                .scale(theme::ICON_SIZE_TINY)
                .padding_right(theme::grid(0.5)),
            Empty,
        ));
    }
    let minor_label = Label::dynamic(move |row: &PlayRow<Arc<Track>>, _| {
        let artist = if display.artist {
            row.item.artist_names()
        } else {
            String::new()
        };
        let album = if display.album {
            let mut album_name = String::new();
            Track::lens_album_name().with(&row.item, |name| album_name = name.to_string());
            album_name
        } else {
            String::new()
        };
        let text = if !artist.is_empty() && !album.is_empty() {
            format!("{artist} • {album}")
        } else {
            format!("{artist}{album}")
        };
        if row.item.is_missing_local_file() {
            format!("Not found in local folders • {text}")
        } else {
            text
        }
    })
    .with_line_break_mode(LineBreaking::Clip)
    .with_text_size(theme::TEXT_SIZE_SMALL)
    .with_text_color(theme::PLACEHOLDER_COLOR);

    minor_row.add_flex_child(minor_label, 1.0);
    minor.add_flex_child(minor_row, 1.0);

    if display.popularity {
        let track_popularity = Label::<Arc<Track>>::dynamic(|track, _| {
            track.popularity.map(popularity_stars).unwrap_or_default()
        })
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .lens(PlayRow::item);
        major.add_default_spacer();
        major.add_child(track_popularity);
    }

    let track_duration =
        Label::<Arc<Track>>::dynamic(|track, _| utils::as_minutes_and_seconds(track.duration))
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .with_text_color(theme::PLACEHOLDER_COLOR)
            .lens(PlayRow::item);
    major.add_default_spacer();
    major.add_child(track_duration);

    let saved = ViewSwitcher::new(
        |row: &PlayRow<Arc<Track>>, _| row.ctx.library.saved_tracks.is_resolved(),
        |selector: &bool, _, _| match selector {
            true => ViewSwitcher::new(
                |row: &PlayRow<Arc<Track>>, _| row.ctx.library.contains_track(&row.item),
                |selector: &bool, _, _| {
                    match selector {
                        true => &icons::CIRCLE_CHECK,
                        false => &icons::CIRCLE_PLUS,
                    }
                    .scale(theme::ICON_SIZE_SMALL)
                    .boxed()
                },
            )
            .on_left_click(|ctx, _, row, _| {
                let track = &row.item;
                if row.ctx.library.contains_track(track) {
                    ctx.submit_command(library::UNSAVE_TRACK.with(track.id))
                } else {
                    ctx.submit_command(library::SAVE_TRACK.with(track.clone()))
                }
            })
            .boxed(),
            false => Box::new(Flex::column()),
        },
    );
    // Local files can't be saved to the library.
    let saved = Either::new(
        |row: &PlayRow<Arc<Track>>, _| row.item.is_local,
        Empty,
        saved,
    );

    main_row
        .with_flex_child(
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(major)
                .with_spacer(2.0)
                .with_child(minor)
                .on_left_click(|ctx, _, row, _| {
                    if !row.item.is_missing_local_file() {
                        ctx.submit_notification(cmd::PLAY.with(row.position))
                    }
                }),
            1.0,
        )
        .with_default_spacer()
        .with_child(saved.center())
        .env_scope(|env, row: &PlayRow<Arc<Track>>| {
            // Grey out local items that aren't in the local folders, and the
            // tracks not to play.
            if row.item.is_missing_local_file() || row.ctx.library.is_hidden(&row.item.id) {
                env.set(theme::TEXT_COLOR, env.get(theme::GREY_400));
                env.set(theme::PLACEHOLDER_COLOR, env.get(theme::GREY_400));
                env.set(theme::ICON_COLOR, env.get(theme::GREY_500));
            }
        })
        .padding(theme::grid(1.0))
        .link()
        .active(|row: &PlayRow<Arc<Track>>, _env: &Env| {
            // Check if this track is the target of album detail navigation
            if let Nav::AlbumDetail(_, Some(target_id)) = &row.ctx.nav {
                return *target_id == row.item.id;
            }
            row.is_playing
        })
        .rounded(theme::BUTTON_BORDER_RADIUS)
        // Follows the playing track down the list, when the list is on screen.
        .on_update(|ctx, old: &PlayRow<Arc<Track>>, row, _| {
            if row.is_playing && !old.is_playing {
                ctx.scroll_to_view();
            }
        })
        .context_menu(track_row_menu)
}

fn cover_widget(size: f64) -> impl Widget<Arc<Track>> {
    RemoteImage::new(placeholder_widget(), move |track: &Arc<Track>, _| {
        track
            .album
            .as_ref()
            .and_then(|al| al.image(size, size).map(|image| image.url.clone()))
    })
    .fix_size(size, size)
}

fn rounded_cover_widget(size: f64) -> impl Widget<Arc<Track>> {
    cover_widget(size).clip(Size::new(size, size).to_rounded_rect(4.0))
}

fn popularity_stars(popularity: u32) -> String {
    const COUNT: usize = 5;

    let popularity_coef = popularity as f32 / 100.0;
    let popular = (COUNT as f32 * popularity_coef).round() as usize;
    let unpopular = COUNT - popular;

    let mut stars = String::with_capacity(COUNT);
    for _ in 0..popular {
        stars.push('★');
    }
    for _ in 0..unpopular {
        stars.push('☆');
    }
    stars
}

fn track_row_menu(row: &PlayRow<Arc<Track>>) -> Menu<AppState> {
    track_menu(&row.item, &row.ctx.library, &row.origin)
}

pub fn track_menu(
    track: &Arc<Track>,
    library: &Library,
    origin: &PlaybackOrigin,
) -> Menu<AppState> {
    if track.is_local {
        return local_track_menu(track, origin);
    }

    let mut menu = Menu::empty();

    for artist_link in &track.artists {
        let more_than_one_artist = track.artists.len() > 1;
        let title = if more_than_one_artist {
            LocalizedString::new("menu-item-show-artist-name")
                .with_placeholder(format!("Go to Artist \"{}\"", artist_link.name))
        } else {
            LocalizedString::new("menu-item-show-artist").with_placeholder("Go to Artist")
        };
        menu = menu.entry(
            MenuItem::new(title)
                .command(cmd::NAVIGATE.with(Nav::ArtistDetail(artist_link.to_owned()))),
        );
    }

    if let Some(album_link) = track.album.as_ref() {
        menu = menu.entry(
            MenuItem::new(
                LocalizedString::new("menu-item-show-album").with_placeholder("Go to Album"),
            )
            .command(cmd::NAVIGATE.with(Nav::AlbumDetail(album_link.to_owned(), None))),
        );
    }

    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-show-credits").with_placeholder("Show Track Credits"),
        )
        .command(cmd::SHOW_CREDITS_WINDOW.with(track.clone())),
    );

    menu = menu.separator();

    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-copy-link").with_placeholder("Copy Link to Track"),
        )
        .command(cmd::COPY.with(track.url())),
    );

    if library.contains_track(track) {
        menu = menu.entry(
            MenuItem::new(
                LocalizedString::new("menu-item-remove-from-library")
                    .with_placeholder("Remove Track from Library"),
            )
            .command(library::UNSAVE_TRACK.with(track.id)),
        );
    } else {
        menu = menu.entry(
            MenuItem::new(
                LocalizedString::new("menu-item-save-to-library")
                    .with_placeholder("Save Track to Library"),
            )
            .command(library::SAVE_TRACK.with(track.clone())),
        );
    }

    if let PlaybackOrigin::Playlist(playlist) = origin {
        // Do some (hopefully) quick checks to determine if we should give the
        // option to remove items from this playlist, only allowing it if the
        // playlist is collaborative or we are the owner of it
        let should_show = {
            if let Some(details) = library
                .playlists
                .resolved()
                .and_then(|pl| pl.iter().find(|p| p.id == playlist.id))
            {
                if details.collaborative {
                    true
                } else if let Some(user) = library.user_profile.resolved() {
                    user.id == details.owner.id
                } else {
                    // If we can find the playlist, but for some reason can't
                    // resolve our own user, just show the option anyways and
                    // we'll see an error at the bottom if it doesn't work
                    // when they try to remove a track
                    true
                }
            } else {
                // If this playlist doesn't exist in our library,
                // just assume that we can't edit it since we probably
                // searched for it or something
                false
            }
        };

        if should_show {
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-item-remove-from-playlist")
                        .with_placeholder("Remove from Current Playlist"),
                )
                .command(playlist::REMOVE_TRACK.with(PlaylistRemoveTrack {
                    link: playlist.to_owned(),
                    track_uri: format!("spotify:track:{}", track.id.0.to_base62()).into(),
                })),
            );
        }
    }

    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-add-to-queue").with_placeholder("Add Track to Queue"),
        )
        // PlayerCommand
        .command(add_to_queue_command(track, origin)),
    );

    menu = menu.separator();
    for entry in hide_entries(track, library) {
        menu = menu.entry(entry);
    }

    let mut playlist_menu = Menu::new(
        LocalizedString::new("menu-item-add-to-playlist").with_placeholder("Add to Playlist"),
    );
    for playlist in library.writable_playlists() {
        playlist_menu = playlist_menu.entry(
            MenuItem::new(
                LocalizedString::new("menu-item-save-to-playlist")
                    .with_placeholder(format!("{}", playlist.name)),
            )
            .command(playlist::ADD_TRACK.with(PlaylistAddTrack {
                link: playlist.link(),
                track_id: track.id,
            })),
        );
    }
    menu = menu.entry(playlist_menu);

    menu
}

/// Not playing `track` for 30 days or for good, or playing it again.
pub fn hide_entries(track: &Track, library: &Library) -> Vec<MenuItem<AppState>> {
    const SNOOZE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    if library.is_hidden(&track.id) {
        return vec![MenuItem::new("Play Again").command(cmd::UNHIDE_TRACK.with(track.id))];
    }
    vec![
        MenuItem::new("Don't Play for 30 Days")
            .command(cmd::HIDE_TRACK.with((track.id, Some(SNOOZE)))),
        MenuItem::new("Never Play").command(cmd::HIDE_TRACK.with((track.id, None))),
    ]
}

fn add_to_queue_command(track: &Arc<Track>, origin: &PlaybackOrigin) -> Command {
    cmd::ADD_TO_QUEUE.with((
        QueueEntry {
            item: crate::ui::Playable::Track(track.clone()),
            origin: origin.clone(),
        },
        PlaybackItem {
            // Keep the ID type, local files are not Spotify tracks.
            item_id: track.id.0,
            norm_level: NormalizationLevel::Track,
        },
    ))
}

/// Local files have no Spotify ID, so everything that goes through the Web API
/// (library, credits, links, playlists) is left out.
fn local_track_menu(track: &Arc<Track>, origin: &PlaybackOrigin) -> Menu<AppState> {
    let mut menu = Menu::empty();

    match &track.local_path {
        Some(path) => {
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-item-add-to-queue")
                        .with_placeholder("Add Track to Queue"),
                )
                .command(add_to_queue_command(track, origin)),
            );
            menu = menu.separator();
            if let Some(folder) = Path::new(path.as_ref()).parent() {
                menu = menu.entry(
                    MenuItem::new(
                        LocalizedString::new("menu-item-show-in-folder")
                            .with_placeholder("Show File in Folder"),
                    )
                    .command(cmd::GO_TO_URL.with(folder.to_string_lossy().into_owned())),
                );
            }
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-item-copy-path").with_placeholder("Copy File Path"),
                )
                .command(cmd::COPY.with(path.to_string())),
            );
        }
        None => {
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-item-local-files-preferences")
                        .with_placeholder("Not Found — Set Up Local Folders…"),
                )
                .command(commands::SHOW_PREFERENCES),
            );
        }
    }

    menu
}
