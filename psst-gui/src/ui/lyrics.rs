use std::time::Duration;

use druid::widget::{
    Button, Container, CrossAxisAlignment, Flex, Label, LineBreaking, List, Scroll, TextBox,
};
use druid::{
    lens::Map, widget::Either, Data, FontDescriptor, FontFamily, FontWeight, Insets, LensExt,
    Selector, Widget, WidgetExt,
};

use crate::cmd;
use crate::controller::InputController;
use crate::data::{
    AppState, Ctx, Lyrics, LyricsCandidate, LyricsSearch, NowPlaying, Playable, TrackLines,
};
use crate::error::Error;
use crate::widget::{Empty, MyWidgetExt};
use crate::{webapi::WebApi, widget::Async};

use super::theme;
use super::utils;

pub const SHOW_LYRICS: Selector<NowPlaying> = Selector::new("app.home.show_lyrics");
const SEARCH_LYRICS: Selector<String> = Selector::new("app.lyrics.search");
const CHOOSE_LYRICS: Selector<LyricsCandidate> = Selector::new("app.lyrics.choose");

/// Playback progress is reported about once a second, so highlight lines a
/// bit early to not lag behind the singing.
const HIGHLIGHT_LOOKAHEAD: Duration = Duration::from_millis(400);

pub fn lyrics_widget() -> impl Widget<AppState> {
    Scroll::new(
        Container::new(
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Center)
                .with_default_spacer()
                .with_child(track_info_widget())
                .with_spacer(theme::grid(2.0))
                .with_child(search_widget())
                .with_child(search_results_widget())
                .with_spacer(theme::grid(2.0))
                .with_child(track_lyrics_widget())
                .with_spacer(theme::grid(2.0)),
        )
        .expand_width()
        .padding(Insets::uniform_xy(theme::grid(4.0), 0.0)),
    )
    .vertical()
}

fn track_info_widget() -> impl Widget<AppState> {
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .with_child(
            Label::dynamic(|data: &AppState, _| {
                data.playback.now_playing.as_ref().map_or_else(
                    || "No track playing".to_string(),
                    |np| match &np.item {
                        Playable::Track(track) => track.name.clone().to_string(),
                        _ => "Unknown track".to_string(),
                    },
                )
            })
            .with_font(theme::UI_FONT_MEDIUM)
            .with_text_size(theme::TEXT_SIZE_LARGE),
        )
        .with_spacer(theme::grid(0.5))
        .with_child(
            Label::dynamic(|data: &AppState, _| {
                data.playback.now_playing.as_ref().map_or_else(
                    || "".to_string(),
                    |np| match &np.item {
                        Playable::Track(track) => {
                            format!("{} - {}", track.artist_name(), track.album_name())
                        }
                        _ => "".to_string(),
                    },
                )
            })
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .with_text_color(theme::PLACEHOLDER_COLOR),
        )
}

/// Search box for picking other lyrics from LRCLIB when the automatic match
/// is missing or wrong.
fn search_widget() -> impl Widget<AppState> {
    let input = TextBox::new()
        .with_placeholder("Search lyrics (artist and title)")
        .controller(InputController::new().on_submit(|ctx, query, _| {
            if !query.trim().is_empty() {
                ctx.submit_command(SEARCH_LYRICS.with(query.clone()));
            }
        }))
        .lens(AppState::lyrics_search.then(LyricsSearch::query))
        .expand_width();
    let button = Button::new("Search").on_click(|ctx, data: &mut AppState, _| {
        let query = data.lyrics_search.query.trim().to_string();
        if !query.is_empty() {
            ctx.submit_command(SEARCH_LYRICS.with(query));
        }
    });
    Flex::row()
        .with_flex_child(input, 1.0)
        .with_default_spacer()
        .with_child(button)
}

fn search_results_widget() -> impl Widget<AppState> {
    let result = || {
        Flex::column()
            .cross_axis_alignment(CrossAxisAlignment::Start)
            .with_child(
                Label::dynamic(|c: &LyricsCandidate, _| {
                    format!("{} — {}", c.track_name, c.artist_name)
                })
                .with_font(theme::UI_FONT_MEDIUM)
                .with_line_break_mode(LineBreaking::WordWrap),
            )
            .with_spacer(2.0)
            .with_child(
                Label::dynamic(|c: &LyricsCandidate, _| {
                    let kind = if c.instrumental {
                        "instrumental"
                    } else if c.is_synced() {
                        "synced"
                    } else {
                        "plain"
                    };
                    let length = utils::as_minutes_and_seconds(Duration::from_secs_f64(
                        c.duration.max(0.0),
                    ));
                    if c.album_name.is_empty() {
                        format!("{length} • {kind}")
                    } else {
                        format!("{} • {length} • {kind}", c.album_name)
                    }
                })
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .with_text_color(theme::PLACEHOLDER_COLOR)
                .with_line_break_mode(LineBreaking::WordWrap),
            )
            .expand_width()
            .padding(theme::grid(1.0))
            .link()
            .rounded(theme::BUTTON_BORDER_RADIUS)
            .on_left_click(|ctx, _, candidate: &mut LyricsCandidate, _| {
                ctx.submit_command(CHOOSE_LYRICS.with(candidate.clone()));
            })
    };

    Async::new(
        utils::spinner_widget,
        move || {
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_spacer(theme::grid(1.0))
                .with_child(
                    Label::dynamic(|results: &druid::im::Vector<LyricsCandidate>, _| {
                        if results.is_empty() {
                            "No results.".to_string()
                        } else {
                            "Pick the right lyrics:".to_string()
                        }
                    })
                    .with_text_size(theme::TEXT_SIZE_SMALL)
                    .with_text_color(theme::PLACEHOLDER_COLOR),
                )
                .with_child(List::new(result))
        },
        || {
            Label::dynamic(|err: &String, _| format!("Search failed: {err}"))
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .with_text_color(theme::PLACEHOLDER_COLOR)
                .padding(Insets::new(0.0, theme::grid(1.0), 0.0, 0.0))
        },
    )
    .lens(AppState::lyrics_search.then(LyricsSearch::results))
    .on_command_async(
        SEARCH_LYRICS,
        |query| {
            WebApi::global()
                .search_lrclib(&query)
                .map_err(|err| err.to_string())
        },
        |_, data, query| data.lyrics_search.results.defer(query),
        |_, data, r| data.lyrics_search.results.update(r),
    )
    .on_command(CHOOSE_LYRICS, |_, candidate, data| {
        if let Some(Playable::Track(track)) = data.playback.now_playing.as_ref().map(|np| &np.item)
        {
            WebApi::global().remember_lyrics_choice(track, candidate);
        }
        data.lyrics.resolve((), crate::webapi::lyrics_from_candidate(candidate));
        data.lyrics_search.results.clear();
    })
}

/// What a lyrics line needs to know to highlight itself.
#[derive(Clone, Data)]
struct Playhead {
    progress: Duration,
    synced: bool,
}

fn playhead_lens() -> impl druid::Lens<AppState, Playhead> {
    Map::new(
        |data: &AppState| Playhead {
            progress: data
                .playback
                .now_playing
                .as_ref()
                .map(|np| np.progress)
                .unwrap_or_default(),
            synced: data.lyrics.resolved().is_some_and(|lyrics| lyrics.synced),
        },
        |_, _| {
            // Read-only.
        },
    )
}

fn line_position(line: &Ctx<Playhead, TrackLines>) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let position = (line.ctx.progress + HIGHLIGHT_LOOKAHEAD).as_millis() as u64;
    if position < line.data.start_ms() {
        Ordering::Greater // Still to come.
    } else if position < line.data.end_ms() {
        Ordering::Equal // Being sung.
    } else {
        Ordering::Less // Already sung.
    }
}

fn is_current_line(line: &Ctx<Playhead, TrackLines>) -> bool {
    line.ctx.synced && line_position(line).is_eq()
}

/// Big bold lines, like Spotify.  The sung lines stay readable, the current
/// one is the brightest and the upcoming ones are dimmed.
#[derive(Clone, Copy)]
struct LineStyle {
    font_size: f64,
    /// Spacing kept around the current line when scrolling to it.
    scroll_margin: f64,
    /// The small panel has no search box of its own.
    in_panel: bool,
}

const MAIN_VIEW_STYLE: LineStyle = LineStyle {
    font_size: 28.0,
    scroll_margin: theme::GRID * 16.0,
    in_panel: false,
};

const PANEL_STYLE: LineStyle = LineStyle {
    font_size: 18.0,
    scroll_margin: theme::GRID * 6.0,
    in_panel: true,
};

fn lyrics_line_widget(style: LineStyle) -> impl Widget<Ctx<Playhead, TrackLines>> {
    let font = FontDescriptor::new(FontFamily::SYSTEM_UI)
        .with_weight(FontWeight::BOLD)
        .with_size(style.font_size);
    Label::raw()
        .with_font(font)
        .with_line_break_mode(LineBreaking::WordWrap)
        .lens(Ctx::data().then(TrackLines::words))
        .expand_width()
        .padding(Insets::uniform_xy(theme::grid(1.0), theme::grid(0.75)))
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .env_scope(|env, line: &Ctx<Playhead, TrackLines>| {
            if !line.ctx.synced {
                return;
            }
            match line_position(line) {
                std::cmp::Ordering::Less => {
                    env.set(theme::TEXT_COLOR, env.get(theme::PLACEHOLDER_COLOR))
                }
                std::cmp::Ordering::Greater => {
                    env.set(theme::TEXT_COLOR, env.get(theme::GREY_400))
                }
                std::cmp::Ordering::Equal => {}
            }
        })
        .on_update(move |ctx, old, new, _| {
            if is_current_line(new) && !is_current_line(old) {
                // Keep some lines of context around the current one.
                ctx.scroll_area_to_view(ctx.size().to_rect().inflate(0.0, style.scroll_margin));
            }
        })
        .on_left_click(|ctx, _, line, _| {
            let start = line.data.start_ms();
            if start != 0 {
                ctx.submit_command(cmd::SKIP_TO_POSITION.with(start));
            }
        })
}

fn lyrics_lines_widget(style: LineStyle) -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        move || {
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(
                    List::new(move || lyrics_line_widget(style)).lens(Ctx::map(Lyrics::lines)),
                )
                .with_spacer(theme::grid(2.0))
                .with_child(
                    Label::dynamic(|lyrics: &Ctx<Playhead, Lyrics>, _| {
                        format!("Lyrics from {}", lyrics.data.source)
                    })
                    .with_text_size(theme::TEXT_SIZE_SMALL)
                    .with_text_color(theme::PLACEHOLDER_COLOR)
                    .padding(Insets::uniform_xy(theme::grid(1.0), 0.0)),
                )
        },
        move || {
            Label::dynamic(move |err: &Error, _| match err {
                Error::WebApiError(message) if message.contains("busy") => {
                    format!("{message}.")
                }
                _ if style.in_panel => {
                    "No lyrics found. Open the lyrics view to search for them.".to_string()
                }
                _ => "No lyrics found for this track. Try searching above.".to_string(),
            })
            .with_line_break_mode(LineBreaking::WordWrap)
            .with_text_alignment(druid::TextAlignment::Center)
            .expand_width()
        },
    )
    .lens(Ctx::make(playhead_lens(), AppState::lyrics).then(Ctx::in_promise()))
}

fn track_lyrics_widget() -> impl Widget<AppState> {
    lyrics_lines_widget(MAIN_VIEW_STYLE)
}

/// Loads the lyrics of the playing track into `AppState::lyrics`.  Attached
/// to the window root, so they also load for the small panel alone.
pub fn lyrics_loader<W: Widget<AppState> + 'static>(widget: W) -> impl Widget<AppState> {
    widget.on_command_async(
        SHOW_LYRICS,
        |np: NowPlaying| match &np.item {
            Playable::Track(track) => WebApi::global().lyrics_for_track(track),
            Playable::Episode(_) => Err(Error::WebApiError("No lyrics for episodes".to_string())),
        },
        |_, data, np| {
            data.lyrics.defer(());
            // Start a manual search from the track, in case it's needed.
            data.lyrics_search.results.clear();
            data.lyrics_search.query = match &np.item {
                Playable::Track(track) => format!("{} {}", track.artist_name(), track.name),
                Playable::Episode(_) => String::new(),
            };
        },
        |_, data, r| data.lyrics.update(((), r.1)),
    )
}

/// Small lyrics panel floating over the bottom right corner of the main
/// window, toggled from the playback bar, so the lyrics stay visible while
/// browsing.
pub fn lyrics_panel() -> impl Widget<AppState> {
    let title = Label::dynamic(|data: &AppState, _| {
        data.playback
            .now_playing
            .as_ref()
            .and_then(|np| np.item.track())
            .map(|track| format!("{} — {}", track.name, track.artist_name()))
            .unwrap_or_else(|| "Nothing playing".to_string())
    })
    .with_text_size(theme::TEXT_SIZE_SMALL)
    .with_text_color(theme::PLACEHOLDER_COLOR)
    .with_line_break_mode(LineBreaking::Clip);
    let close = Label::new("✕")
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .padding(Insets::uniform_xy(theme::grid(0.75), theme::grid(0.25)))
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .on_left_click(|ctx, _, _, _| ctx.submit_command(cmd::TOGGLE_LYRICS_WINDOW));
    let header = Flex::row()
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .with_flex_child(title, 1.0)
        .with_child(close)
        .padding(Insets::new(
            theme::grid(1.5),
            theme::grid(1.0),
            theme::grid(1.0),
            theme::grid(0.5),
        ));
    let lines = Scroll::new(lyrics_lines_widget(PANEL_STYLE).padding(Insets::new(
        theme::grid(0.5),
        0.0,
        theme::grid(0.5),
        theme::grid(1.5),
    )))
    .vertical()
    .expand();
    let panel = Flex::column()
        .with_child(header)
        .with_flex_child(lines, 1.0)
        .fix_size(theme::grid(42.0), theme::grid(30.0))
        .background(theme::BACKGROUND_DARK)
        .rounded(theme::GRID)
        .border(theme::GREY_500, 1.0);
    Either::new(|data: &AppState, _| data.lyrics_panel_open, panel, Empty)
}
