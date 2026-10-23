use crate::data::config::SortCriteria;
use crate::data::Track;
use crate::error::Error;
use crate::{
    cmd,
    controller::{
        AlertCleanupController, LocalFilesController, NavController,
        SessionController, SortController,
    },
    data::{
        config::SortOrder, Alert, AlertStyle, AppState, Config, Nav, Playable, Playback,
        PlaybackOrigin, PlaylistLink, Route,
    },
    webapi::WebApi,
    widget::{
        icons, icons::SvgIcon, Border, CornerOverlay, Empty, MyWidgetExt, Overlay, RemoteImage,
        ThemeScope, ViewDispatcher,
    },
};
use credits::TrackCredits;
use druid::widget::Controller;
use druid::KbKey;
use druid::{
    im::Vector,
    widget::{
        CrossAxisAlignment, Either, Flex, Label, LineBreaking, List, Scroll, Slider, Split,
        ViewSwitcher,
    },
    Color, Env, Insets, Key, LensExt, Menu, MenuItem, Selector, Widget, WidgetExt, WindowDesc,
};
use druid_shell::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

pub mod album;
pub mod artist;
pub mod credits;
pub mod episode;
pub mod find;
pub mod home;
pub mod library;
pub mod lyrics;
pub mod playable;
pub mod playback;
pub mod playlist;
pub mod preferences;
pub mod queue;
pub mod search;
pub mod show;
pub mod stats;
pub mod theme;
pub mod track;
pub mod user;
pub mod utils;

pub const DOWNLOAD_ARTWORK: Selector<(String, String)> = Selector::new("app.artwork.download");

/// Keeps the window's size for the next start, and to go back to from the mini
/// player.  As Windows creates windows, with the title bar and the borders:
/// resizes report the inside, so the window shrank on every start.
struct KeepWindowSize;

impl<W: Widget<AppState>> Controller<AppState, W> for KeepWindowSize {
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut druid::EventCtx,
        event: &druid::Event,
        data: &mut AppState,
        env: &Env,
    ) {
        match (event, data.mini_mode) {
            (druid::Event::WindowConnected, _) => keep_min_size(ctx.window()),
            // Not a minimized or maximized size, to open with next time.
            (druid::Event::WindowSize(_), false)
                if ctx.window().get_window_state() == druid::WindowState::Restored =>
            {
                data.config.window_size = ctx.window().get_size();
            }
            _ => {}
        }
        child.event(ctx, event, data, env)
    }
}

/// The smallest window the full layout fits in: the sidebar's links with a few
/// playlists, and the playback bar's buttons.
const MIN_WINDOW_SIZE: (f64, f64) = (theme::GRID * 90.0, theme::GRID * 70.0);

/// Whether the window shows the full layout, not the mini player.
static FULL_LAYOUT: AtomicBool = AtomicBool::new(true);

/// Holds the full layout at its minimum size. Druid sets a minimum only when
/// the window opens, so that one is the mini player's.
fn keep_min_size(window: &druid::WindowHandle) {
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::{
            HiDpi::GetDpiForWindow,
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::{MINMAXINFO, WM_GETMINMAXINFO},
        },
    };

    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _: usize,
        _: usize,
    ) -> LRESULT {
        // SAFETY: passes the message on to druid's own handling.
        let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        if msg == WM_GETMINMAXINFO && FULL_LAYOUT.load(Ordering::Relaxed) {
            let dpi = f64::from(unsafe { GetDpiForWindow(hwnd) });
            let px = |dp: f64| (dp * dpi / 96.0) as i32;
            // SAFETY: this message's lparam points to the window's MINMAXINFO.
            let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            info.ptMinTrackSize.x = px(MIN_WINDOW_SIZE.0);
            info.ptMinTrackSize.y = px(MIN_WINDOW_SIZE.1);
        }
        result
    }

    if let RawWindowHandle::Win32(handle) = window.raw_window_handle() {
        // SAFETY: the window is open, and `proc` passes on every message.
        unsafe { SetWindowSubclass(handle.hwnd, Some(proc), 1, 0) };
    }
    // A size saved smaller opens as it was.
    let size = window.get_size();
    window.set_size(druid::Size::new(
        size.width.max(MIN_WINDOW_SIZE.0),
        size.height.max(MIN_WINDOW_SIZE.1),
    ));
}

/// The window turned into the mini player, its title bar included.
pub const MINI_PLAYER_SIZE: (f64, f64) = (theme::GRID * 60.0, theme::GRID * 26.0);

pub fn main_window(config: &Config) -> WindowDesc<AppState> {
    // Outermost: druid gives the window's size only to its root widget.
    WindowDesc::new(root_widget().controller(KeepWindowSize))
        .title(compute_main_window_title)
        // The mini player's; `keep_min_size` holds the full layout's.
        .with_min_size(MINI_PLAYER_SIZE)
        .window_size(config.window_size)
        .show_title(false)
        .transparent_titlebar(true)
}

pub fn preferences_window() -> WindowDesc<AppState> {
    let win_size = (theme::grid(60.0), theme::grid(55.0));

    // On Windows, the window size includes the titlebar.
    let win_size = (win_size.0, win_size.1 + 56.0);

    WindowDesc::new(preferences_widget())
        .title("Preferences")
        .window_size(win_size)
        .resizable(false)
        .show_title(false)
        .transparent_titlebar(true)
}

pub fn account_setup_window() -> WindowDesc<AppState> {
    let win_size = (theme::grid(50.0), theme::grid(45.0));

    // On Windows, the window size includes the titlebar, so without this offset
    // the login button gets clipped off the bottom of the window.
    let win_size = (win_size.0, win_size.1 + 56.0);

    WindowDesc::new(account_setup_widget())
        .title("Login")
        .window_size(win_size)
        .with_min_size(win_size)
        .resizable(true)
        .show_title(false)
        .transparent_titlebar(true)
}

pub fn artwork_window() -> WindowDesc<AppState> {
    let win_size = (theme::grid(50.0), theme::grid(50.0));

    // On Windows, the window size includes the titlebar, so we need to account for it
    let win_size = (win_size.0, win_size.1 + 24.0);

    WindowDesc::new(artwork_widget())
        .window_size(win_size)
        .resizable(false)
        .show_title(false)
        .transparent_titlebar(true)
        .title(|data: &AppState, _env: &_| {
            data.playback
                .now_playing
                .as_ref()
                .map(|np| match &np.item {
                    Playable::Track(track) => {
                        format!("{} - {}", track.album_name(), track.artist_name())
                    }
                    Playable::Episode(episode) => episode.name.to_string(),
                })
                .unwrap_or_else(|| "Now Playing".to_string())
        })
}

fn preferences_widget() -> impl Widget<AppState> {
    ThemeScope::new(
        preferences::preferences_widget()
            .background(theme::BACKGROUND_DARK)
            .expand(),
    )
}

fn account_setup_widget() -> impl Widget<AppState> {
    ThemeScope::new(
        preferences::account_setup_widget()
            .background(theme::BACKGROUND_DARK)
            .expand(),
    )
}

struct ArtworkController;

impl<W: Widget<AppState>> Controller<AppState, W> for ArtworkController {
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut druid::EventCtx,
        event: &druid::Event,
        data: &mut AppState,
        env: &druid::Env,
    ) {
        if let druid::Event::WindowConnected = event {
            ctx.request_focus();
            ctx.set_handled();
        }

        if let druid::Event::KeyDown(key_event) = event {
            // Handle D key for download
            if key_event.key == KbKey::Character('d'.into()) {
                if let Some(np) = &data.playback.now_playing {
                    if let Some((url, _)) = np.cover_image_metadata() {
                        let title = match &np.item {
                            Playable::Track(track) => track
                                .album
                                .as_ref()
                                .map(|a| a.name.as_ref())
                                .unwrap_or("Unknown Album"),
                            Playable::Episode(episode) => episode.show.name.as_ref(),
                        };
                        ctx.submit_command(
                            DOWNLOAD_ARTWORK.with((url.to_string(), title.to_string())),
                        );
                        ctx.set_handled();
                    }
                }
            }
        }
        child.event(ctx, event, data, env);
    }
}

pub fn artwork_widget() -> impl Widget<AppState> {
    RemoteImage::new(utils::placeholder_widget(), move |data: &AppState, _| {
        data.playback
            .now_playing
            .as_ref()
            .and_then(|np| np.cover_image_url(512.0, 512.0))
            .map(|url| url.into())
    })
    .expand()
    .background(theme::BACKGROUND_DARK)
    .controller(ArtworkController)
}

fn root_widget() -> impl Widget<AppState> {
    let playlists = Scroll::new(playlist::list_widget())
        .vertical()
        .expand_height();

    let playlists = Flex::column()
        .must_fill_main_axis(true)
        .with_child(sidebar_menu_widget())
        .with_default_spacer()
        .with_child(playlist::sort_widget())
        .with_child(playlist::new_playlist_widget().align_left())
        .with_flex_child(playlists, 1.0);

    let controls = Flex::column()
        .with_default_spacer()
        .with_child(volume_slider())
        .with_default_spacer()
        .with_child(user::user_widget())
        .center()
        .fix_height(88.0)
        .background(Border::Top.with_color(theme::GREY_500));

    let sidebar = Flex::column()
        .with_child(library::saved_tracks_loader())
        .with_flex_child(playlists, 1.0)
        .with_child(download_status_widget())
        .with_child(controls)
        .background(theme::BACKGROUND_DARK);

    let topbar = Flex::row()
        .must_fill_main_axis(true)
        .with_child(topbar_back_button_widget())
        .with_flex_child(topbar_title_widget(), 1.0)
        .with_child(topbar_sort_widget())
        .background(Border::Bottom.with_color(theme::BACKGROUND_DARK));

    let main = Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(topbar)
        .with_flex_child(
            Flex::row()
                .must_fill_main_axis(true)
                .cross_axis_alignment(CrossAxisAlignment::Fill)
                .with_flex_child(
                    Overlay::bottom(
                        CornerOverlay::new(
                            route_widget(),
                            lyrics::lyrics_panel(),
                            theme::grid(2.0),
                        ),
                        alert_widget(),
                    ),
                    1.0,
                )
                .with_child(queue::panel_widget()),
            1.0,
        )
        .with_child(selection_bar())
        .with_child(playback::panel_widget())
        .background(theme::BACKGROUND_LIGHT);

    let split = Split::columns(sidebar, main)
        .split_point(0.2)
        .bar_size(1.0)
        .min_size(150.0, 300.0)
        .min_bar_area(1.0)
        .solid_bar(true);

    // The player is in the full layout, which still gets the commands while the
    // mini player shows.
    let layout = Either::new(
        |data: &AppState, _| data.mini_mode,
        playback::mini_player_widget(),
        split,
    )
    .on_command(cmd::TOGGLE_MINI_PLAYER, |ctx, _, data: &mut AppState| {
        data.mini_mode = !data.mini_mode;
        data.mini_lyrics_open = false;
        FULL_LAYOUT.store(!data.mini_mode, Ordering::Relaxed);
        let window = ctx.window();
        window.set_always_on_top(data.mini_mode);
        window.set_size(if data.mini_mode {
            MINI_PLAYER_SIZE.into()
        } else {
            data.config.window_size
        });
    });
    let root = ThemeScope::new(layout)
        .controller(SessionController)
        .controller(NavController)
        .controller(SortController)
        .controller(LocalFilesController::new())
        .on_command_async(
            cmd::LOAD_TRACK_CREDITS,
            |track: Arc<Track>| {
                log::debug!("fetching credits for track: {}", track.name);
                WebApi::global().get_track_credits(&track.id.0.to_base62())
            },
            |_, data: &mut AppState, _| {
                data.credits = None;
            },
            |_ctx, data, (_track, result): (Arc<Track>, Result<TrackCredits, Error>)| match result {
                Ok(credits) => {
                    data.credits = Some(credits);
                }
                Err(err) => {
                    log::error!("Failed to fetch credits for {}: {:?}", _track.name, err);
                    data.error_alert(format!("Failed to fetch track credits: {err}"));
                }
            },
        );
    // .debug_invalidation()
    // .debug_widget_id()
    // .debug_paint_layout()
    lyrics::lyrics_loader(root)
}

fn alert_widget() -> impl Widget<AppState> {
    const BG: Key<Color> = Key::new("app.alert.BG");
    const DISMISS_ALERT: Selector<usize> = Selector::new("app.alert.dismiss");

    List::new(|| {
        Flex::row()
            .with_child(
                Label::dynamic(|alert: &Alert, _| match alert.style {
                    AlertStyle::Error => "Error:".to_string(),
                    AlertStyle::Info => String::new(),
                })
                .with_font(theme::UI_FONT_MEDIUM),
            )
            .with_default_spacer()
            .with_flex_child(Label::raw().lens(Alert::message), 1.0)
            .padding(theme::grid(2.0))
            .background(BG)
            .env_scope(|env, alert: &Alert| {
                env.set(
                    BG,
                    match alert.style {
                        AlertStyle::Error => env.get(theme::RED),
                        AlertStyle::Info => env.get(theme::GREY_600),
                    },
                )
            })
    })
    .lens(AppState::alerts)
    .on_command(DISMISS_ALERT, |_, &id, state| {
        state.dismiss_alert(id);
    })
    .controller(AlertCleanupController)
}

fn route_widget() -> impl Widget<AppState> {
    ViewDispatcher::new(
        |state: &AppState, _| state.nav.route(),
        |route: &Route, _, _| match route {
            Route::Home => Scroll::new(home::home_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::Lyrics => Scroll::new(lyrics::lyrics_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::SavedTracks => Flex::column()
                .with_child(
                    find::finder_widget(cmd::FIND_IN_SAVED_TRACKS, "Find in Saved Tracks...")
                        .lens(AppState::finder),
                )
                .with_flex_child(
                    Scroll::new(library::saved_tracks_widget().padding(theme::grid(1.0)))
                        .vertical(),
                    1.0,
                )
                .boxed(),
            Route::SavedAlbums => {
                Scroll::new(library::saved_albums_widget().padding(theme::grid(1.0)))
                    .vertical()
                    .boxed()
            }
            Route::SavedArtists => {
                Scroll::new(library::followed_artists_widget().padding(theme::grid(1.0)))
                    .vertical()
                    .boxed()
            }
            Route::ArtistSavedTracks => {
                Scroll::new(artist::saved_tracks_page().padding(theme::grid(1.0)))
                    .vertical()
                    .boxed()
            }
            Route::Shows => Scroll::new(library::saved_shows_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::NewReleases => {
                Scroll::new(library::new_releases_widget().padding(theme::grid(1.0)))
                    .vertical()
                    .boxed()
            }
            Route::Stats => Scroll::new(stats::stats_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::SearchResults => search::results_widget().padding(theme::grid(1.0)).boxed(),
            Route::AlbumDetail => Scroll::new(album::detail_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::ArtistDetail => Scroll::new(artist::detail_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
            Route::PlaylistDetail => Flex::column()
                .with_child(
                    find::finder_widget(cmd::FIND_IN_PLAYLIST, "Find in Playlist...")
                        .lens(AppState::finder),
                )
                .with_flex_child(
                    Scroll::new(playlist::detail_widget().padding(theme::grid(1.0))).vertical(),
                    1.0,
                )
                .boxed(),
            Route::ShowDetail => Scroll::new(show::detail_widget().padding(theme::grid(1.0)))
                .vertical()
                .boxed(),
        },
    )
    .expand()
}

/// "Downloading 3 of 80" and the track being saved, while offline downloads run.
/// What to do with the tracks picked with Ctrl and Shift clicks.
fn selection_bar() -> impl Widget<AppState> {
    let selected = |data: &AppState| -> Vector<Arc<Track>> {
        data.common_ctx.selection().into_iter().collect()
    };
    let action = |label: &str| {
        Label::new(label)
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .padding(Insets::uniform_xy(theme::grid(1.0), theme::grid(0.5)))
            .link()
            .rounded(theme::BUTTON_BORDER_RADIUS)
    };
    let download = action("Download").on_left_click(move |ctx, _, data: &mut AppState, _| {
        let tracks = selected(data);
        let name = format!("{} selected songs", tracks.len()).into();
        ctx.submit_command(cmd::DOWNLOAD_TRACKS.with((name, tracks)));
        data.common_ctx_mut().selected.clear();
    });
    let like = action("Like").on_left_click(move |ctx, _, data: &mut AppState, _| {
        ctx.submit_command(library::LIKE_TRACKS.with(selected(data)));
        data.common_ctx_mut().selected.clear();
    });
    let queue = action("Queue").on_left_click(move |ctx, _, data: &mut AppState, _| {
        let origin = selection_origin(&data.nav);
        for track in selected(data) {
            ctx.submit_command(track::add_to_queue_command(&track, &origin));
        }
        data.common_ctx_mut().selected.clear();
    });
    let add_to_playlist =
        action("Add to Playlist").on_left_click(move |ctx, mouse, data: &mut AppState, _| {
            let tracks = selected(data);
            let mut menu = Menu::<AppState>::new("Add to Playlist");
            for playlist in data.library.writable_playlists() {
                let add = playlist::ADD_TRACKS.with((playlist.link(), tracks.clone()));
                menu = menu.entry(MenuItem::new(playlist.name.to_string()).command(add));
            }
            ctx.show_context_menu(menu, mouse.window_pos);
        });
    // Out of the list on screen: Liked Songs, or a playlist you can edit.
    let remove = Either::new(
        |data: &AppState, _| removal(data).is_some(),
        action("Remove").on_left_click(move |ctx, _, data: &mut AppState, _| {
            let tracks = selected(data);
            match removal(data) {
                Some(Some(link)) => {
                    ctx.submit_command(playlist::REMOVE_TRACKS.with((link, tracks)))
                }
                Some(None) => ctx.submit_command(library::UNLIKE_TRACKS.with(tracks)),
                None => return,
            }
            data.common_ctx_mut().selected.clear();
        }),
        Empty,
    );
    let clear = action("Clear").on_left_click(|_, _, data: &mut AppState, _| {
        data.common_ctx_mut().selected.clear();
    });
    let bar = Flex::row()
        .with_child(
            Label::dynamic(|data: &AppState, _| {
                format!("{} selected", data.common_ctx.selected.len())
            })
            .with_text_size(theme::TEXT_SIZE_SMALL),
        )
        .with_flex_spacer(1.0)
        .with_child(queue)
        .with_child(add_to_playlist)
        .with_child(like)
        .with_child(remove)
        .with_child(download)
        .with_child(clear)
        .padding(Insets::uniform_xy(theme::grid(2.0), theme::grid(0.5)))
        .background(Border::Top.with_color(theme::GREY_500));
    Either::new(
        |data: &AppState, _| data.common_ctx.selected.is_empty(),
        Empty,
        bar,
    )
    .on_command(cmd::SELECT_TRACKS, |_, (tracks, range), data| {
        data.common_ctx_mut().select(tracks, *range);
    })
}

/// Where removing selected tracks takes them out of: the playlist on screen when
/// you can edit it (`Some(Some)`), Liked Songs (`Some(None)`), or nowhere.
fn removal(data: &AppState) -> Option<Option<PlaylistLink>> {
    match &data.nav {
        Nav::SavedTracks | Nav::ArtistSavedTracks(_) => Some(None),
        Nav::PlaylistDetail(link) => {
            let writable = data.library.writable_playlists();
            writable
                .iter()
                .any(|p| p.id == link.id)
                .then(|| Some(link.clone()))
        }
        _ => None,
    }
}

/// What queued tracks say they play from.
fn selection_origin(nav: &Nav) -> PlaybackOrigin {
    match nav {
        Nav::SavedTracks => PlaybackOrigin::Library,
        Nav::PlaylistDetail(link) => PlaybackOrigin::Playlist(link.clone()),
        Nav::AlbumDetail(link, _) => PlaybackOrigin::Album(link.clone()),
        _ => PlaybackOrigin::Home,
    }
}

fn download_status_widget() -> impl Widget<AppState> {
    Either::new(
        |data: &AppState, _| data.download_queue.is_empty(),
        Empty,
        Flex::column()
            .cross_axis_alignment(CrossAxisAlignment::Start)
            .with_child(
                Flex::row()
                    .with_child(
                        Label::dynamic(|data: &AppState, _| {
                            let current = data.download_total - data.download_queue.len() + 1;
                            format!("Downloading {current} of {}", data.download_total)
                        })
                        .with_text_size(theme::TEXT_SIZE_SMALL),
                    )
                    .with_flex_spacer(1.0)
                    .with_child(
                        Label::new("Stop")
                            .with_text_size(theme::TEXT_SIZE_SMALL)
                            .link()
                            .on_left_click(|ctx, _, _, _| {
                                ctx.submit_command(cmd::CANCEL_DOWNLOADS)
                            }),
                    )
                    .must_fill_main_axis(true),
            )
            .with_child(
                Label::dynamic(|data: &AppState, _| {
                    data.download_queue
                        .front()
                        .map(|track| track.name.to_string())
                        .unwrap_or_default()
                })
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .with_text_color(theme::PLACEHOLDER_COLOR)
                .with_line_break_mode(LineBreaking::Clip),
            )
            .padding(theme::grid(1.0))
            .expand_width()
            .background(Border::Top.with_color(theme::GREY_500)),
    )
}

fn sidebar_menu_widget() -> impl Widget<AppState> {
    Flex::column()
        .with_default_spacer()
        .with_child(sidebar_link_widget("Home", Some(&icons::HOME), Nav::Home))
        .with_child(sidebar_link_widget(
            "Tracks",
            Some(&icons::MUSIC_NOTE),
            Nav::SavedTracks,
        ))
        .with_child(sidebar_link_widget(
            "Albums",
            Some(&icons::ALBUM),
            Nav::SavedAlbums,
        ))
        .with_child(sidebar_link_widget(
            "Artists",
            Some(&icons::ARTIST),
            Nav::SavedArtists,
        ))
        .with_child(sidebar_link_widget(
            "Podcasts",
            Some(&icons::PODCAST),
            Nav::Shows,
        ))
        .with_child(sidebar_link_widget(
            "New Releases",
            Some(&icons::STAR),
            Nav::NewReleases,
        ))
        .with_child(sidebar_link_widget(
            "Stats",
            Some(&icons::STATS),
            Nav::Stats,
        ))
        .with_child(search::input_widget().padding((theme::grid(1.0), theme::grid(1.0))))
}

fn sidebar_link_widget(
    title: &str,
    icon: Option<&icons::SvgIcon>,
    link_nav: Nav,
) -> impl Widget<AppState> {
    Flex::row()
        .with_child(
            icon.map(|i| {
                i.scale((18.0, 18.0))
                    .padding_right(theme::grid(1.0))
                    .boxed()
            })
            .unwrap_or_else(|| Empty.boxed()),
        )
        .with_child(Label::new(title))
        .with_flex_spacer(1.0)
        .padding((theme::grid(2.0), theme::grid(1.0)))
        .expand_width()
        .link()
        .env_scope({
            let link_nav = link_nav.clone();
            move |env, nav: &Nav| {
                env.set(
                    theme::LINK_COLD_COLOR,
                    if &link_nav == nav {
                        env.get(theme::MENU_BUTTON_BG_ACTIVE)
                    } else {
                        env.get(theme::MENU_BUTTON_BG_INACTIVE)
                    },
                );
                env.set(
                    theme::TEXT_COLOR,
                    if &link_nav == nav {
                        env.get(theme::MENU_BUTTON_FG_ACTIVE)
                    } else {
                        env.get(theme::MENU_BUTTON_FG_INACTIVE)
                    },
                );
            }
        })
        .on_left_click(move |ctx, _, _, _| {
            ctx.submit_command(cmd::NAVIGATE.with(link_nav.clone()));
        })
        .lens(AppState::nav)
}

fn volume_slider() -> impl Widget<AppState> {
    const SAVE_DELAY: Duration = Duration::from_millis(100);
    const SAVE_TO_CONFIG: Selector = Selector::new("app.volume.save-to-config");

    Flex::row()
        .with_flex_child(
            Slider::new()
                .with_range(0.0, 1.0)
                .expand_width()
                .env_scope(|env, _| {
                    env.set(theme::BASIC_WIDGET_HEIGHT, theme::grid(1.5));
                    env.set(theme::FOREGROUND_LIGHT, env.get(theme::GREY_400));
                    env.set(theme::FOREGROUND_DARK, env.get(theme::GREY_400));
                })
                .with_cursor(Cursor::Pointer),
            1.0,
        )
        .with_default_spacer()
        .with_child(
            Label::dynamic(|&volume: &f64, _| format!("{}%", (volume * 100.0).floor()))
                .with_text_color(theme::PLACEHOLDER_COLOR)
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .fix_width(theme::grid(4.0)),
        )
        .padding((theme::grid(2.0), 0.0))
        .on_debounce(SAVE_DELAY, |ctx, _, _| ctx.submit_command(SAVE_TO_CONFIG))
        .lens(AppState::playback.then(Playback::volume))
        .on_scroll(
            |data| &data.config.slider_scroll_scale,
            |_, data, _, scaled_delta| {
                data.playback.volume = (data.playback.volume + scaled_delta).clamp(0.0, 1.0);
            },
        )
}

fn topbar_sort_widget() -> impl Widget<AppState> {
    let up_icon = icons::UP.scale((10.0, theme::grid(2.0)));
    let down_icon = icons::DOWN.scale((10.0, theme::grid(2.0)));

    // "Sort: Title", a click opens the list of criteria.
    let criteria = Label::dynamic(|data: &AppState, _| {
        let name = match data.current_sort().0 {
            SortCriteria::Title => "Title",
            SortCriteria::Artist => "Artist",
            SortCriteria::Album => "Album",
            SortCriteria::Duration => "Duration",
            SortCriteria::DateAdded => "Date Added",
        };
        format!("Sort: {name}")
    })
    .with_text_size(theme::TEXT_SIZE_SMALL)
    .padding(theme::grid(1.0))
    .link()
    .rounded(theme::BUTTON_BORDER_RADIUS)
    .on_left_click(|ctx, mouse, data: &mut AppState, _| {
        ctx.show_context_menu(sorting_menu(data), mouse.window_pos);
    })
    .context_menu(sorting_menu);

    // Arrow toggling between ascending and descending.
    let direction = Either::new(
        |data: &AppState, _| data.current_sort().1 == SortOrder::Ascending,
        up_icon,
        down_icon,
    )
    .padding(theme::grid(1.0))
    .link()
    .rounded(theme::BUTTON_BORDER_RADIUS)
    .on_left_click(|ctx, _, _, _| {
        ctx.submit_command(cmd::TOGGLE_SORT_ORDER);
    });

    let enabled = Flex::row()
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .with_child(criteria)
        .with_child(direction);

    Either::new(
        |data: &AppState, _| data.nav.track_list().is_some(),
        enabled,
        Empty,
    )
    .padding(theme::grid(1.0))
}

fn topbar_back_button_widget() -> impl Widget<AppState> {
    let icon = icons::BACK.scale((10.0, theme::grid(2.0)));
    let disabled = icon
        .clone()
        .with_color(theme::GREY_600)
        .padding(theme::grid(1.0));
    let enabled = icon
        .padding(theme::grid(1.0))
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .on_left_click(|ctx, _, _, _| {
            ctx.submit_command(cmd::NAVIGATE_BACK.with(1));
        })
        .context_menu(history_menu);
    Either::new(
        |history: &Vector<Nav>, _| history.is_empty(),
        disabled,
        enabled,
    )
    .padding(theme::grid(1.0))
    .lens(AppState::history)
}

fn history_menu(history: &Vector<Nav>) -> Menu<AppState> {
    let mut menu = Menu::empty();

    for (index, history) in history.iter().rev().take(10).enumerate() {
        let skip_back_in_history_n_times = index + 1;
        menu = menu.entry(
            MenuItem::new(history.full_title())
                .command(cmd::NAVIGATE_BACK.with(skip_back_in_history_n_times)),
        );
    }

    menu
}

fn sorting_menu(app_state: &AppState) -> Menu<AppState> {
    let mut menu = Menu::new("Sort by");

    // Create menu items for sorting options
    let mut sort_by_title = MenuItem::new("Title").command(cmd::SORT_BY_TITLE);
    let mut sort_by_album = MenuItem::new("Album").command(cmd::SORT_BY_ALBUM);
    let mut sort_by_date_added = MenuItem::new("Date Added").command(cmd::SORT_BY_DATE_ADDED);
    let mut sort_by_duration = MenuItem::new("Duration").command(cmd::SORT_BY_DURATION);
    let mut sort_by_artist = MenuItem::new("Artist").command(cmd::SORT_BY_ARTIST);

    match app_state.current_sort().0 {
        SortCriteria::Title => sort_by_title = sort_by_title.selected(true),
        SortCriteria::Album => sort_by_album = sort_by_album.selected(true),
        SortCriteria::DateAdded => sort_by_date_added = sort_by_date_added.selected(true),
        SortCriteria::Duration => sort_by_duration = sort_by_duration.selected(true),
        SortCriteria::Artist => sort_by_artist = sort_by_artist.selected(true),
    };

    // Add the items and checkboxes to the menu
    menu = menu.entry(sort_by_album);
    menu = menu.entry(sort_by_artist);
    menu = menu.entry(sort_by_date_added);
    menu = menu.entry(sort_by_duration);
    menu = menu.entry(sort_by_title);

    menu
}

fn topbar_title_widget() -> impl Widget<AppState> {
    Flex::row()
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .with_flex_child(route_title_widget(), 1.0)
        .with_spacer(theme::grid(0.5))
        .with_child(route_icon_widget())
        .lens(AppState::nav)
}

fn route_icon_widget() -> impl Widget<Nav> {
    ViewSwitcher::new(
        |nav: &Nav, _| nav.clone(),
        |nav: &Nav, _, _| {
            let icon = |icon: &SvgIcon| icon.scale(theme::ICON_SIZE_MEDIUM);
            match &nav {
                Nav::Home
                | Nav::NewReleases
                | Nav::Stats
                | Nav::Lyrics
                | Nav::SavedTracks
                | Nav::SavedAlbums
                | Nav::SavedArtists
                | Nav::Shows => Empty.boxed(),
                Nav::SearchResults(_) => icon(&icons::SEARCH).boxed(),
                Nav::AlbumDetail(_, _) => icon(&icons::ALBUM).boxed(),
                Nav::ArtistDetail(_) | Nav::ArtistSavedTracks(_) => icon(&icons::ARTIST).boxed(),
                Nav::PlaylistDetail(_) => icon(&icons::PLAYLIST).boxed(),
                Nav::ShowDetail(_) => icon(&icons::PODCAST).boxed(),
            }
        },
    )
}

fn route_title_widget() -> impl Widget<Nav> {
    Label::dynamic(|nav: &Nav, _| nav.title())
        .with_line_break_mode(LineBreaking::Clip)
        .with_font(theme::UI_FONT_MEDIUM)
        .with_text_size(theme::TEXT_SIZE_LARGE)
}

fn compute_main_window_title(data: &AppState, _env: &Env) -> String {
    if let Some(now_playing) = &data.playback.now_playing {
        match &now_playing.item {
            Playable::Track(track) => {
                format!("{} - {}", track.artist_name(), track.name)
            }
            Playable::Episode(episode) => episode.name.to_string(),
        }
    } else {
        "Psst".to_owned()
    }
}
