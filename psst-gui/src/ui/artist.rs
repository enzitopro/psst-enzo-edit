use druid::{
    kurbo::Circle,
    widget::{
        Button, CrossAxisAlignment, Either, Flex, Label, LabelText, LineBreaking, List, Scroll,
    },
    Data, Insets, LensExt, LocalizedString, Menu, MenuItem, Selector, Size, UnitPoint, Widget,
    WidgetExt,
};

use crate::{
    cmd,
    data::{
        AppState, Artist, ArtistAlbums, ArtistDetail, ArtistInfo, ArtistLink, ArtistOverview,
        Cached, CommonCtx, Ctx, Nav, WithCtx,
    },
    ui::utils::{stat_row, InfoLayout},
    webapi::WebApi,
    widget::{Async, Empty, MyWidgetExt, RemoteImage},
};

use super::{
    album, playable, theme, track,
    utils::{self},
};

pub const LOAD_DETAIL: Selector<ArtistLink> = Selector::new("app.artist.load-detail");

pub fn detail_widget() -> impl Widget<AppState> {
    Flex::column()
        .with_child(async_artist_info().padding((theme::grid(1.0), 0.0)))
        .with_child(async_albums_widget().padding((theme::grid(1.0), 0.0)))
        .with_child(async_related_widget().padding((theme::grid(1.0), 0.0)))
}

/// The artist's tracks in the library, like a playlist: play them, download
/// them all, or go to the artist.
pub fn saved_tracks_page() -> impl Widget<AppState> {
    let artist = |data: &AppState| match &data.nav {
        Nav::ArtistSavedTracks(artist) => Some(artist.clone()),
        _ => None,
    };
    let info = Flex::row()
        .with_child(
            Label::dynamic(
                |data: &AppState, _| match data.artist_detail.saved_tracks.len() {
                    1 => "1 saved song".to_string(),
                    count => format!("{count} saved songs"),
                },
            )
            .with_text_size(theme::TEXT_SIZE_SMALL),
        )
        .with_default_spacer()
        .with_child(
            Label::new("Go to Artist")
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .link()
                .on_left_click(move |ctx, _, data: &mut AppState, _| {
                    if let Some(artist) = artist(data) {
                        ctx.submit_command(cmd::NAVIGATE.with(Nav::ArtistDetail(artist)));
                    }
                }),
        );
    let buttons = Flex::row()
        .with_child(playable::play_buttons(|data| {
            Some(&data.artist_detail.saved_tracks)
        }))
        .with_default_spacer()
        .with_child(Button::new("Download for Offline").on_click(
            move |ctx, data: &mut AppState, _| {
                if let Some(artist) = artist(data) {
                    let tracks = data.artist_detail.saved_tracks.clone();
                    // Remembered like a playlist, for the mark on the Artists page.
                    let ids = tracks
                        .iter()
                        .filter(|t| t.is_downloadable())
                        .map(|t| t.id)
                        .collect();
                    let key = CommonCtx::offline_key(&artist);
                    ctx.submit_command(cmd::OFFLINE_PLAYLIST.with((key, tracks.len(), ids)));
                    ctx.submit_command(cmd::DOWNLOAD_TRACKS.with((artist.name, tracks)));
                }
            },
        ));
    let tracks = playable::list_widget(playable::Display {
        track: track::Display {
            title: true,
            album: true,
            cover: true,
            ..track::Display::empty()
        },
    })
    .lens(Ctx::make(
        AppState::common_ctx,
        AppState::artist_detail.then(ArtistDetail::saved_tracks),
    ));
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(info)
        .with_spacer(theme::grid(1.0))
        .with_child(buttons)
        .with_spacer(theme::grid(1.0))
        .with_child(tracks)
}

fn async_albums_widget() -> impl Widget<AppState> {
    Async::new(utils::spinner_widget, albums_widget, utils::error_widget)
        .lens(
            Ctx::make(
                AppState::common_ctx,
                AppState::artist_detail.then(ArtistDetail::albums),
            )
            .then(Ctx::in_promise()),
        )
        .on_command_async(
            LOAD_DETAIL,
            |d| WebApi::global().get_artist_albums(&d.id),
            |_, data, d| data.artist_detail.albums.defer(d),
            |_, data, r| {
                if let Err(err) = &r.1 {
                    log::error!("failed to load albums of artist {:?}: {err}", r.0.name);
                }
                data.artist_detail.albums.update(r)
            },
        )
}

/// Owns the single artist overview request for the whole page.
fn async_artist_info() -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        || {
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(
                    artist_info_widget().lens(Ctx::map(Cached::data.then(ArtistOverview::info))),
                )
                .with_child(utils::cached_age_widget().lens(Ctx::data()))
        },
        || Empty,
    )
    .lens(
        Ctx::make(
            AppState::common_ctx,
            AppState::artist_detail.then(ArtistDetail::overview),
        )
        .then(Ctx::in_promise()),
    )
    .on_command_async(
        LOAD_DETAIL,
        |d| WebApi::global().get_artist_overview(&d.id),
        |_, data, d| data.artist_detail.overview.defer(d),
        |_, data, r| {
            if let Err(err) = &r.1 {
                log::error!("failed to load artist {:?}: {err}", r.0.name);
            }
            data.artist_detail.overview.update(r)
        },
    )
}

/// Renders part of the promise loaded by `async_artist_info`; intentionally
/// has no `LOAD_DETAIL` handler of its own.
fn async_related_widget() -> impl Widget<AppState> {
    Async::new(utils::spinner_widget, related_widget, utils::error_widget)
        .lens(AppState::artist_detail.then(ArtistDetail::overview))
}

/// `open` is where clicking it goes, e.g. `Nav::ArtistDetail`.
pub fn artist_widget(horizontal: bool, open: fn(ArtistLink) -> Nav) -> impl Widget<Artist> {
    let (mut artist, artist_image) = if horizontal {
        (Flex::column(), cover_widget(theme::grid(16.0)))
    } else {
        (Flex::row(), cover_widget(theme::grid(6.0)))
    };

    artist = if horizontal {
        artist
            .with_child(artist_image)
            .with_default_spacer()
            .with_child(
                Label::raw()
                    .with_font(theme::UI_FONT_MEDIUM)
                    .align_horizontal(UnitPoint::CENTER)
                    .align_vertical(UnitPoint::TOP)
                    .fix_size(theme::grid(16.0), theme::grid(8.0))
                    .lens(Artist::name),
            )
    } else {
        artist
            .with_child(artist_image)
            .with_default_spacer()
            .with_flex_child(
                Label::raw()
                    .with_font(theme::UI_FONT_MEDIUM)
                    .lens(Artist::name),
                1.0,
            )
    };

    artist
        .padding(theme::grid(1.0))
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .on_left_click(move |ctx, _, artist, _| {
            ctx.submit_command(cmd::NAVIGATE.with(open(artist.link())));
        })
        .context_menu(|artist| artist_menu(&artist.link()))
}

pub fn link_widget() -> impl Widget<ArtistLink> {
    Label::raw()
        .with_line_break_mode(LineBreaking::WordWrap)
        .with_font(theme::UI_FONT_MEDIUM)
        .link()
        .lens(ArtistLink::name)
        .on_left_click(|ctx, _, link, _| {
            ctx.submit_command(cmd::NAVIGATE.with(Nav::ArtistDetail(link.to_owned())));
        })
        .context_menu(artist_menu)
}

pub fn cover_widget(size: f64) -> impl Widget<Artist> {
    let radius = size / 2.0;
    RemoteImage::new(utils::placeholder_widget(), move |artist: &Artist, _| {
        artist.image(size, size).map(|image| image.url.clone())
    })
    .fix_size(size, size)
    .clip(Circle::new((radius, radius), radius))
}

fn artist_info_widget() -> impl Widget<WithCtx<ArtistInfo>> {
    let size = theme::grid(16.0);

    let artist_image = RemoteImage::new(
        utils::placeholder_widget(),
        move |artist: &ArtistInfo, _| Some(artist.main_image.clone()),
    )
    .fix_size(size, size)
    .clip(Size::new(size, size).to_rounded_rect(4.0))
    .lens(Ctx::data());

    let biography = Scroll::new(
        Label::new(|data: &ArtistInfo, _env: &_| data.bio.clone())
            .with_line_break_mode(LineBreaking::WordWrap)
            .with_text_size(theme::TEXT_SIZE_NORMAL)
            .lens(Ctx::data()),
    )
    .vertical();

    let artist_stats = Flex::column()
        .with_child(stat_row("Followers:", |info: &ArtistInfo| {
            utils::format_number_with_commas(info.stats.followers)
        }))
        .with_default_spacer()
        .with_child(stat_row("Monthly Listeners:", |info: &ArtistInfo| {
            utils::format_number_with_commas(info.stats.monthly_listeners)
        }))
        .with_default_spacer()
        .with_child(Either::new(
            |ctx: &WithCtx<ArtistInfo>, _| ctx.data.stats.world_rank > 0,
            stat_row("Ranking:", |info: &ArtistInfo| {
                format!(
                    "#{} in the world",
                    utils::format_number_with_commas(info.stats.world_rank)
                )
            }),
            Empty,
        ));

    Flex::row()
        .with_child(artist_image)
        .with_spacer(theme::grid(1.0))
        .with_flex_child(
            Flex::row().with_flex_child(InfoLayout::new(biography, artist_stats), 1.0),
            1.0,
        )
        .context_menu(|artist| artist_info_menu(&artist.data))
        .padding((0.0, theme::grid(1.0))) // Keep overall vertical padding
}

fn albums_widget() -> impl Widget<WithCtx<ArtistAlbums>> {
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(header_widget("Albums"))
        .with_child(album::grid_widget().lens(Ctx::map(ArtistAlbums::albums)))
        .with_child(header_widget("Singles"))
        .with_child(album::grid_widget().lens(Ctx::map(ArtistAlbums::singles)))
        .with_child(header_widget("Compilations"))
        .with_child(album::grid_widget().lens(Ctx::map(ArtistAlbums::compilations)))
        .with_child(header_widget("Appears On"))
        .with_child(album::grid_widget().lens(Ctx::map(ArtistAlbums::appears_on)))
}

fn related_widget() -> impl Widget<Cached<ArtistOverview>> {
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(header_widget("Related Artists"))
        .with_child(List::new(|| artist_widget(false, Nav::ArtistDetail)))
        .lens(Cached::data.then(ArtistOverview::related))
}

fn header_widget<T: Data>(text: impl Into<LabelText<T>>) -> impl Widget<T> {
    Label::new(text)
        .with_font(theme::UI_FONT_MEDIUM)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .padding(Insets::new(0.0, theme::grid(2.0), 0.0, theme::grid(1.0)))
}

fn artist_info_menu(artist: &ArtistInfo) -> Menu<AppState> {
    let mut menu = Menu::empty();

    for artist_link in &artist.artist_links {
        let platform = if artist_link.contains("wikipedia.org") {
            "Wikipedia"
        } else {
            artist_link
                .strip_prefix("https://")
                .unwrap_or(artist_link)
                .split('.')
                .next()
                .unwrap_or("Unknown")
        };

        let title = LocalizedString::new("menu-item-go-to-social").with_placeholder(format!(
            "Go to their {}",
            platform
                .chars()
                .next()
                .unwrap()
                .to_uppercase()
                .collect::<String>()
                + &platform[1..]
        ));

        menu =
            menu.entry(MenuItem::new(title).command(cmd::GO_TO_URL.with(artist_link.to_owned())));
    }

    menu
}

fn artist_menu(artist: &ArtistLink) -> Menu<AppState> {
    let mut menu = Menu::empty();

    menu = menu.entry(
        MenuItem::new(
            LocalizedString::new("menu-item-copy-link").with_placeholder("Copy Link to Artist"),
        )
        .command(cmd::COPY.with(artist.url())),
    );

    menu
}
