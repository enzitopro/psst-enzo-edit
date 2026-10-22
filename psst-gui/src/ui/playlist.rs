use std::{cell::RefCell, rc::Rc, sync::Arc};

use druid::{
    lens::Map,
    widget::{Button, Either, Flex, Label, LensWrap, LineBreaking, List, TextBox},
    Insets, Lens, LensExt, LocalizedString, Menu, MenuItem, Selector, Size, UnitPoint, Widget,
    WidgetExt, WindowDesc,
};

use crate::{
    cmd,
    data::{
        sort_tracks, AppState, Ctx, Library, Nav, Playlist, PlaylistAddTrack, PlaylistDetail,
        PlaylistLink, PlaylistRemoveTrack, PlaylistTracks, WithCtx, CUSTOM_ORDER,
    },
    error::Error,
    webapi::WebApi,
    widget::{icons, Async, Empty, MyWidgetExt, RemoteImage, ThemeScope},
};

use super::{library, playable, theme, track, utils};

/// Loads the sidebar's playlists, in a library sort order.
pub const LOAD_LIST: Selector<String> = Selector::new("app.playlist.load-list");
const SORT_LIST: Selector<String> = Selector::new("app.playlist.sort-list");
/// Moves a playlist up (`true`) or down a place in the library's own order.
const MOVE_PLAYLIST: Selector<(PlaylistLink, bool)> = Selector::new("app.playlist.move");
/// The user, the playlist's URI, and the one to put it before (`true`) or after.
const MOVE_IN_ROOTLIST: Selector<(Arc<str>, String, String, bool)> =
    Selector::new("app.playlist.move-in-rootlist");

/// The library's sort orders, as Spotify names them.
const ORDERS: [&str; 5] = [
    "Recents",
    "Recently Added",
    "Alphabetical",
    "Creator",
    CUSTOM_ORDER,
];
pub const LOAD_DETAIL: Selector<(PlaylistLink, AppState)> =
    Selector::new("app.playlist.load-detail");
pub const ADD_TRACK: Selector<PlaylistAddTrack> = Selector::new("app.playlist.add-track");
pub const REMOVE_TRACK: Selector<PlaylistRemoveTrack> = Selector::new("app.playlist.remove-track");

pub const FOLLOW_PLAYLIST: Selector<Playlist> = Selector::new("app.playlist.follow");
pub const UNFOLLOW_PLAYLIST: Selector<PlaylistLink> = Selector::new("app.playlist.unfollow");
pub const UNFOLLOW_PLAYLIST_CONFIRM: Selector<PlaylistLink> =
    Selector::new("app.playlist.unfollow-confirm");

pub const RENAME_PLAYLIST: Selector<PlaylistLink> = Selector::new("app.playlist.rename");
pub const RENAME_PLAYLIST_CONFIRM: Selector<PlaylistLink> =
    Selector::new("app.playlist.rename-confirm");

const SHOW_RENAME_PLAYLIST_CONFIRM: Selector<PlaylistLink> =
    Selector::new("app.playlist.show-rename");
const SHOW_UNFOLLOW_PLAYLIST_CONFIRM: Selector<UnfollowPlaylist> =
    Selector::new("app.playlist.show-unfollow-confirm");

/// "Sort: Recents" over the sidebar's playlists, a click picks another order.
pub fn sort_widget() -> impl Widget<AppState> {
    Label::dynamic(|data: &AppState, _| format!("Sort: {}", data.config.playlist_order))
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .padding(Insets::uniform_xy(theme::grid(2.0), theme::grid(0.6)))
        .link()
        .on_left_click(|ctx, mouse, data: &mut AppState, _| {
            let mut menu = Menu::<AppState>::new("Sort by");
            for order in ORDERS {
                let item = MenuItem::new(order)
                    .selected(data.config.playlist_order == order)
                    .command(SORT_LIST.with(order.to_string()));
                menu = menu.entry(item);
            }
            ctx.show_context_menu(menu, mouse.window_pos);
        })
        .on_command(SORT_LIST, |ctx, order, data| {
            data.config.playlist_order = order.clone();
            data.common_ctx_mut().playlists_in_custom_order = order == CUSTOM_ORDER;
            ctx.submit_command(LOAD_LIST.with(order.clone()));
        })
        .align_left()
}

pub fn list_widget() -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        || {
            List::new(|| {
                let pin = Either::new(
                    |playlist: &WithCtx<Playlist>, _| playlist.data.pinned,
                    icons::PIN
                        .scale(theme::ICON_SIZE_TINY)
                        .padding_right(theme::grid(0.5)),
                    Empty,
                );
                let name = Label::raw()
                    .with_line_break_mode(LineBreaking::WordWrap)
                    .with_text_size(theme::TEXT_SIZE_SMALL)
                    .lens(Ctx::data().then(Playlist::name));
                let offline = utils::offline_icon(|playlist: &WithCtx<Playlist>| {
                    let data = &playlist.data;
                    playlist.ctx.offline_state(&data.id, data.track_count)
                })
                .padding_left(theme::grid(0.5));
                Flex::row()
                    .cross_axis_alignment(druid::widget::CrossAxisAlignment::Center)
                    .with_child(pin)
                    .with_flex_child(name, 1.0)
                    .with_child(offline)
                    .expand_width()
                    .padding(Insets::uniform_xy(theme::grid(2.0), theme::grid(0.6)))
                    .link()
                    .on_left_click(|ctx, _, playlist, _| {
                        ctx.submit_command(
                            cmd::NAVIGATE.with(Nav::PlaylistDetail(playlist.data.link())),
                        );
                    })
                    .context_menu(playlist_menu_ctx)
            })
        },
        utils::error_widget,
    )
    .lens(
        Ctx::make(
            AppState::common_ctx,
            AppState::library.then(Library::playlists.in_arc()),
        )
        .then(Ctx::in_promise()),
    )
    .on_command_async(
        LOAD_LIST,
        |order| WebApi::global().get_playlists(&order),
        |_, data, _| {
            // Sorting again keeps showing the list meanwhile.
            if !data.library.playlists.is_resolved() {
                data.with_library_mut(|l| l.playlists.defer_default());
            }
        },
        |_, data, (_, r)| data.with_library_mut(|l| l.playlists.resolve_or_reject((), r)),
    )
    .on_command_async(
        ADD_TRACK,
        |d| {
            WebApi::global().add_track_to_playlist(
                &d.link.id,
                &d.track_id
                    .0
                    .to_uri()
                    .ok_or_else(|| Error::WebApiError("Item doesn't have URI".to_string()))?,
            )
        },
        |_, data, d| {
            data.with_library_mut(|library| library.increment_playlist_track_count(&d.link))
        },
        |_, data, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Added to playlist.");
            }
        },
    )
    .on_command_async(
        UNFOLLOW_PLAYLIST,
        |link| WebApi::global().unfollow_playlist(link.id.as_ref()),
        |_, data: &mut AppState, d| data.with_library_mut(|l| l.remove_from_playlist(&d.id)),
        |_, data, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Playlist removed from library.");
            }
        },
    )
    .on_command_async(
        FOLLOW_PLAYLIST,
        |link| WebApi::global().follow_playlist(link.id.as_ref()),
        |_, data: &mut AppState, d| data.with_library_mut(|l| l.add_playlist(d)),
        |_, data: &mut AppState, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Playlist added to library.")
            }
        },
    )
    .on_command_async(
        RENAME_PLAYLIST,
        |link| WebApi::global().change_playlist_details(link.id.as_ref(), link.name.as_ref()),
        |_, data: &mut AppState, link| data.with_library_mut(|l| l.rename_playlist(link)),
        |_, data: &mut AppState, (_, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Playlist renamed.")
            }
        },
    )
    .on_command(SHOW_UNFOLLOW_PLAYLIST_CONFIRM, |ctx, msg, _| {
        let window = unfollow_confirm_window(msg.clone());
        ctx.new_window(window);
    })
    .on_command(MOVE_PLAYLIST, |ctx, (link, up), data: &mut AppState| {
        let user = data
            .library
            .user_profile
            .resolved()
            .map(|user| user.id.clone());
        let playlists = data.library.playlists.resolved();
        let (Some(user), Some(playlists)) = (user, playlists) else {
            return;
        };
        // Pinned ones stay on top, the others move among themselves.
        let movable: Vec<_> = playlists.iter().filter(|p| !p.pinned).collect();
        let Some(index) = movable.iter().position(|p| p.id == link.id) else {
            return;
        };
        let neighbour = if *up {
            index.checked_sub(1).and_then(|i| movable.get(i))
        } else {
            movable.get(index + 1)
        };
        if let Some(neighbour) = neighbour {
            let uri = |id: &str| format!("spotify:playlist:{id}");
            let target = (user, uri(&link.id), uri(&neighbour.id), *up);
            ctx.submit_command(MOVE_IN_ROOTLIST.with(target));
        }
    })
    .on_command_async(
        MOVE_IN_ROOTLIST,
        |(user, uri, anchor, before)| {
            WebApi::global().move_in_rootlist(&user, &uri, &anchor, before)
        },
        |_, _, _| {},
        |ctx, data, (_, result)| match result {
            Ok(()) => ctx.submit_command(LOAD_LIST.with(CUSTOM_ORDER.to_string())),
            Err(err) => data.error_alert(format!("Couldn't move the playlist: {err}")),
        },
    )
    .on_command(SHOW_RENAME_PLAYLIST_CONFIRM, |ctx, link, _| {
        let window = rename_playlist_window(link.clone());
        ctx.new_window(window);
    })
    .on_command_async(
        REMOVE_TRACK,
        |d| WebApi::global().remove_track_from_playlist(&d.link.id, &d.track_uri),
        |_, data, d| {
            data.with_library_mut(|library| library.decrement_playlist_track_count(&d.link))
        },
        |e, data, (p, r)| {
            if let Err(err) = r {
                data.error_alert(err);
            } else {
                data.info_alert("Removed from playlist.");
            }
            // Re-submit the `LOAD_DETAIL` command to reload the playlist data.
            e.submit_command(LOAD_DETAIL.with((p.link, data.clone())))
        },
    )
}

fn unfollow_confirm_window(msg: UnfollowPlaylist) -> WindowDesc<AppState> {
    WindowDesc::new(unfollow_playlist_confirm_widget(msg))
        .window_size((theme::grid(45.0), theme::grid(25.0)))
        .title("Unfollow playlist")
        .resizable(false)
        .show_title(false)
        .transparent_titlebar(true)
}

fn unfollow_playlist_confirm_widget(msg: UnfollowPlaylist) -> impl Widget<AppState> {
    let link = msg.link;

    let information_section = if msg.created_by_user {
        information_section(
            format!("Delete {} from Library?", link.name).as_str(),
            "This will delete the playlist from Your Library",
        )
    } else {
        information_section(
            format!("Remove {} from Library?", link.name).as_str(),
            "We'll remove this playlist from Your Library, but you'll still be able to search for it on Spotify",
        )
    };

    let button_section = button_section(
        "Delete",
        UNFOLLOW_PLAYLIST_CONFIRM,
        Box::new(move || link.clone()),
    );

    ThemeScope::new(
        Flex::column()
            .with_child(information_section)
            .with_flex_spacer(2.0)
            .with_child(button_section)
            .with_flex_spacer(2.0)
            .background(theme::BACKGROUND_DARK),
    )
}

fn rename_playlist_window(link: PlaylistLink) -> WindowDesc<AppState> {
    WindowDesc::new(rename_playlist_widget(link))
        .window_size((theme::grid(45.0), theme::grid(30.0)))
        .title("Rename playlist")
        .resizable(false)
        .show_title(false)
        .transparent_titlebar(true)
}

#[derive(Clone, Lens)]
struct TextInput {
    input: Rc<RefCell<String>>,
}

impl Lens<AppState, String> for TextInput {
    fn with<V, F: FnOnce(&String) -> V>(&self, _data: &AppState, f: F) -> V {
        f(&self.input.borrow())
    }

    fn with_mut<V, F: FnOnce(&mut String) -> V>(&self, _data: &mut AppState, f: F) -> V {
        f(&mut self.input.borrow_mut())
    }
}

fn rename_playlist_widget(link: PlaylistLink) -> impl Widget<AppState> {
    let text_input = TextInput {
        input: Rc::new(RefCell::new(link.name.to_string())),
    };

    let information_section = information_section(
        "Rename playlist?",
        "Please enter a new name for your playlist",
    );
    let input_section = LensWrap::new(
        TextBox::new()
            .padding_horizontal(theme::grid(2.0))
            .expand_width(),
        text_input.clone(),
    );
    let button_section = button_section(
        "Rename",
        RENAME_PLAYLIST_CONFIRM,
        Box::new(move || PlaylistLink {
            id: link.id.clone(),
            name: Arc::from(text_input.input.borrow().clone().into_boxed_str()),
        }),
    );

    ThemeScope::new(
        Flex::column()
            .with_child(information_section)
            .with_child(input_section)
            .with_flex_spacer(2.0)
            .with_child(button_section)
            .with_flex_spacer(2.0)
            .background(theme::BACKGROUND_DARK),
    )
}

fn button_section(
    action_button_name: &str,
    selector: Selector<PlaylistLink>,
    link_extractor: Box<dyn Fn() -> PlaylistLink>,
) -> impl Widget<AppState> {
    let action_button = Button::new(action_button_name)
        .fix_height(theme::grid(5.0))
        .fix_width(theme::grid(9.0))
        .on_click(move |ctx, _, _| {
            ctx.submit_command(selector.with(link_extractor()));
            ctx.window().close();
        });
    let cancel_button = Button::new("Cancel")
        .fix_height(theme::grid(5.0))
        .fix_width(theme::grid(8.0))
        .padding_left(theme::grid(3.0))
        .padding_right(theme::grid(2.0))
        .on_click(|ctx, _, _| ctx.window().close());

    Flex::row()
        .with_child(action_button)
        .with_child(cancel_button)
        .align_right()
}

fn information_section(title_msg: &str, description_msg: &str) -> impl Widget<AppState> {
    let title_label = Label::new(title_msg)
        .with_text_size(theme::TEXT_SIZE_LARGE)
        .align_left()
        .padding(theme::grid(2.0));

    let description_label = Label::new(description_msg)
        .with_line_break_mode(LineBreaking::WordWrap)
        .with_text_size(theme::TEXT_SIZE_NORMAL)
        .align_left()
        .padding(theme::grid(2.0));

    Flex::column()
        .with_child(title_label)
        .with_child(description_label)
}

pub fn playlist_widget(horizontal: bool) -> impl Widget<WithCtx<Playlist>> {
    let playlist_image_size = if horizontal {
        theme::grid(16.0)
    } else {
        theme::grid(6.0)
    };
    let playlist_image = rounded_cover_widget(playlist_image_size).lens(Ctx::data());

    let playlist_name = Label::raw()
        .with_font(theme::UI_FONT_MEDIUM)
        .with_line_break_mode(LineBreaking::Clip)
        .lens(Ctx::data().then(Playlist::name));

    let playlist_description = Label::raw()
        .with_line_break_mode(LineBreaking::WordWrap)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .lens(Ctx::data().then(Playlist::description));

    let (playlist_name, playlist_description) = if horizontal {
        (
            playlist_name.fix_width(playlist_image_size).align_left(),
            playlist_description
                .fix_width(playlist_image_size)
                .align_left(),
        )
    } else {
        (
            playlist_name.align_left(),
            playlist_description.align_left(),
        )
    };

    let playlist = if horizontal {
        Flex::column()
            .with_child(playlist_image)
            .with_default_spacer()
            .with_child(
                Flex::column()
                    .with_child(playlist_name)
                    .with_spacer(2.0)
                    .with_child(playlist_description)
                    .align_horizontal(UnitPoint::CENTER)
                    .align_vertical(UnitPoint::TOP)
                    .fix_size(theme::grid(16.0), theme::grid(8.0)),
            )
            .padding(theme::grid(1.0))
    } else {
        Flex::row()
            .with_child(playlist_image)
            .with_default_spacer()
            .with_flex_child(
                Flex::column()
                    .with_child(playlist_name)
                    .with_spacer(2.0)
                    .with_child(playlist_description),
                1.0,
            )
            .padding(theme::grid(1.0))
    };

    playlist
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .on_left_click(|ctx, _, playlist, _| {
            ctx.submit_command(cmd::NAVIGATE.with(Nav::PlaylistDetail(playlist.data.link())));
        })
        .context_menu(playlist_menu_ctx)
}

fn cover_widget(size: f64) -> impl Widget<Playlist> {
    RemoteImage::new(
        utils::placeholder_widget(),
        move |playlist: &Playlist, _| playlist.image(size, size).map(|image| image.url.clone()),
    )
    .fix_size(size, size)
}

fn rounded_cover_widget(size: f64) -> impl Widget<Playlist> {
    // TODO: Take the radius from theme.
    cover_widget(size).clip(Size::new(size, size).to_rounded_rect(4.0))
}

pub fn detail_widget() -> impl Widget<AppState> {
    use druid::widget::CrossAxisAlignment;

    let playlist_top = async_playlist_info_widget().padding(theme::grid(1.0));

    let playlist_tracks = async_tracks_widget();

    let downloaded_only = Label::dynamic(|data: &AppState, _| {
        if data.common_ctx.downloaded_only {
            "Show All Songs".to_string()
        } else {
            "Show Downloaded Only".to_string()
        }
    })
    .with_text_size(theme::TEXT_SIZE_SMALL)
    .link()
    .on_left_click(|_, _, data: &mut AppState, _| {
        let common = data.common_ctx_mut();
        common.downloaded_only = !common.downloaded_only;
    });
    let play_buttons = Flex::row()
        .with_child(playable::play_buttons(|data| {
            data.playlist_detail.tracks.resolved()
        }))
        .with_default_spacer()
        .with_child(downloaded_only)
        .padding((theme::grid(1.0), 0.0));

    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_spacer(theme::grid(1.0))
        .with_child(playlist_top)
        .with_spacer(theme::grid(1.0))
        .with_child(play_buttons)
        .with_spacer(theme::grid(1.0))
        .with_child(playlist_tracks)
}

fn async_playlist_info_widget() -> impl Widget<AppState> {
    Async::new(utils::spinner_widget, playlist_info_widget, || Empty)
        .lens(
            Ctx::make(
                AppState::common_ctx,
                AppState::playlist_detail.then(PlaylistDetail::playlist),
            )
            .then(Ctx::in_promise()),
        )
        .on_command_async(
            LOAD_DETAIL,
            |d| WebApi::global().get_playlist(&d.0.id),
            |_, data, d| data.playlist_detail.playlist.defer(d.0),
            |_, data, (d, r)| data.playlist_detail.playlist.update((d.0, r)),
        )
}

fn playlist_info_widget() -> impl Widget<WithCtx<Playlist>> {
    use druid::widget::CrossAxisAlignment;

    let size = theme::grid(10.0);
    let playlist_cover = cover_widget(size)
        .lens(Ctx::data())
        .clip(Size::new(size, size).to_rounded_rect(4.0))
        .context_menu(playlist_menu_ctx);

    let owner_label = Label::dynamic(|p: &Playlist, _| p.owner.display_name.as_ref().to_string());

    let track_count_label = Label::dynamic(|p: &Playlist, _| {
        let count = p.track_count.unwrap_or(0);
        if count == 1 {
            "1 song".to_string()
        } else {
            format!("{count} songs")
        }
    })
    .with_text_size(theme::TEXT_SIZE_SMALL);

    let description_widget = Either::new(
        |p: &Playlist, _| !p.description.is_empty(),
        Flex::column().with_default_spacer().with_child(
            Label::dynamic(|p: &Playlist, _| p.description.to_string())
                .with_line_break_mode(LineBreaking::Clip)
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .with_text_color(theme::PLACEHOLDER_COLOR),
        ),
        Empty,
    );

    let visibility_widget = Either::new(
        |p: &Playlist, _| p.public.is_some() || p.collaborative,
        Flex::column().with_default_spacer().with_child(
            Label::dynamic(|p: &Playlist, _| {
                let mut parts = Vec::new();
                match p.public {
                    Some(true) => parts.push("Public"),
                    Some(false) => parts.push("Private"),
                    None => {}
                }
                if p.collaborative {
                    parts.push("Collaborative");
                }
                parts.join(" • ")
            })
            .with_line_break_mode(LineBreaking::WordWrap)
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .with_text_color(theme::PLACEHOLDER_COLOR),
        ),
        Empty,
    );

    let playlist_info = Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(owner_label)
        .with_default_spacer()
        .with_child(track_count_label)
        .with_child(description_widget)
        .with_child(visibility_widget);

    Flex::row()
        .with_child(playlist_cover)
        .with_default_spacer()
        .with_flex_child(playlist_info.lens(Ctx::data()), 1.0)
}

fn async_tracks_widget() -> impl Widget<AppState> {
    Async::new(utils::spinner_widget, tracks_widget, utils::error_widget)
        .lens(
            Ctx::make(
                AppState::common_ctx,
                AppState::playlist_detail.then(PlaylistDetail::tracks),
            )
            .then(Ctx::in_promise()),
        )
        .on_command_async(
            LOAD_DETAIL,
            |arg: (PlaylistLink, AppState)| {
                let d = arg.0;
                let data = arg.1;
                // Errors are shown, not turned into an empty (blank) list.
                let (criteria, order) = data.config.track_sort(&d.id);
                WebApi::global()
                    .get_playlist_tracks(&d.id)
                    .map(|tracks| sort_tracks(&tracks, criteria, order))
            },
            |_, data, d| data.playlist_detail.tracks.defer(d.0),
            |_, data, (d, r)| {
                let tracks = r.map(|tracks| PlaylistTracks {
                    id: d.0.id.clone(),
                    name: d.0.name.clone(),
                    tracks,
                });
                data.playlist_detail.tracks.update((d.0, tracks))
            },
        )
}

fn tracks_widget() -> impl Widget<WithCtx<PlaylistTracks>> {
    let list = playable::list_widget_with_find(
        playable::Display {
            track: track::Display {
                title: true,
                artist: true,
                album: true,
                cover: true,
                ..track::Display::empty()
            },
        },
        cmd::FIND_IN_PLAYLIST,
    );
    // Only what plays without a connection, when asked.
    list.lens(Map::new(
        |playlist: &WithCtx<PlaylistTracks>| {
            let mut shown = playlist.clone();
            if playlist.ctx.downloaded_only {
                let ctx = &playlist.ctx;
                shown.data.tracks.retain(|track| ctx.plays_offline(track));
            }
            shown
        },
        |_, _| {},
    ))
}

fn playlist_menu_ctx(playlist: &WithCtx<Playlist>) -> Menu<AppState> {
    let library = &playlist.ctx.library;
    let custom_order = playlist.ctx.playlists_in_custom_order;
    let playlist = &playlist.data;

    let mut menu = Menu::empty();

    // Only in the library's own order, which Spotify keeps.
    if custom_order && !playlist.pinned && library.contains_playlist(playlist) {
        menu = menu
            .entry(MenuItem::new("Move Up").command(MOVE_PLAYLIST.with((playlist.link(), true))))
            .entry(MenuItem::new("Move Down").command(MOVE_PLAYLIST.with((playlist.link(), false))))
            .separator();
    }

    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-copy-link").with_placeholder("Copy Link to Playlist"),
        )
        .command(cmd::COPY.with(playlist.url())),
    );
    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-download").with_placeholder("Download for Offline"),
        )
        .command(cmd::DOWNLOAD_PLAYLIST.with(playlist.link())),
    );
    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-like-all").with_placeholder("Like All Songs"),
        )
        .command(library::LIKE_PLAYLIST.with(playlist.link())),
    );

    if library.contains_playlist(playlist) {
        let created_by_user = library.is_created_by_user(playlist);

        if created_by_user {
            let unfollow_msg = UnfollowPlaylist {
                link: playlist.link(),
                created_by_user,
            };
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-unfollow-playlist")
                        .with_placeholder("Delete playlist"),
                )
                .command(SHOW_UNFOLLOW_PLAYLIST_CONFIRM.with(unfollow_msg)),
            );
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-rename-playlist")
                        .with_placeholder("Rename playlist"),
                )
                .command(SHOW_RENAME_PLAYLIST_CONFIRM.with(playlist.link())),
            );
        } else {
            let unfollow_msg = UnfollowPlaylist {
                link: playlist.link(),
                created_by_user,
            };
            menu = menu.entry(
                MenuItem::new(
                    LocalizedString::new("menu-unfollow-playlist")
                        .with_placeholder("Remove playlist from Your Library"),
                )
                .command(SHOW_UNFOLLOW_PLAYLIST_CONFIRM.with(unfollow_msg)),
            );
        }
    } else {
        menu = menu.entry(
            MenuItem::new(
                LocalizedString::new("menu-follow-playlist").with_placeholder("Follow Playlist"),
            )
            .command(FOLLOW_PLAYLIST.with(playlist.clone())),
        );
    }

    menu
}

#[derive(Clone)]
struct UnfollowPlaylist {
    link: PlaylistLink,
    created_by_user: bool,
}
