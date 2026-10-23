use std::time::{Duration, Instant};

use druid::widget::{
    Button, Container, Controller, CrossAxisAlignment, Flex, Label, LineBreaking, List, Scroll,
    TextBox,
};
use druid::{
    commands::SCROLL_TO_VIEW, lens::Map, widget::Either, BoxConstraints, Data, Env, Event,
    EventCtx, FontDescriptor, FontFamily, FontWeight, Insets, LayoutCtx, LensExt, LifeCycle,
    LifeCycleCtx, PaintCtx, Selector, Size, TimerToken, UnitPoint, UpdateCtx, Widget, WidgetExt,
};

use crate::cmd;
use crate::controller::InputController;
use crate::data::{
    AppState, Ctx, Lyrics, LyricsCandidate, LyricsSearch, NowPlaying, Playable, PlaybackState,
    TrackLines,
};
use crate::error::Error;
use crate::widget::{Empty, MyWidgetExt};
use crate::{webapi::WebApi, widget::Async};

use super::theme;
use super::utils;

pub const SHOW_LYRICS: Selector<NowPlaying> = Selector::new("app.home.show_lyrics");
const SEARCH_LYRICS: Selector<String> = Selector::new("app.lyrics.search");
const CHOOSE_LYRICS: Selector<LyricsCandidate> = Selector::new("app.lyrics.choose");
/// Scrolls back to the line being sung, and keeps it in view again.
const FOLLOW_LYRICS: Selector = Selector::new("app.lyrics.follow");

/// Lines light up a beat early, as reading one takes a moment.
const HIGHLIGHT_LOOKAHEAD: Duration = Duration::from_millis(150);

pub fn lyrics_widget() -> impl Widget<AppState> {
    lyrics_scroll(
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
}

/// The lyrics in a scroll that keeps the line being sung in view, until the
/// user scrolls away to read, with a way back under it.
fn lyrics_scroll(content: impl Widget<AppState> + 'static) -> impl Widget<AppState> {
    let back = Either::new(
        |data: &AppState, _| {
            data.lyrics_scrolled && data.lyrics.resolved().is_some_and(|lyrics| lyrics.synced)
        },
        Label::new("Back to the current line")
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .padding(Insets::uniform_xy(theme::grid(1.5), theme::grid(0.75)))
            .link()
            .rounded(theme::BUTTON_BORDER_RADIUS)
            .border(theme::GREY_500, 1.0)
            .on_left_click(|ctx, _, _, _| ctx.submit_command(FOLLOW_LYRICS))
            .padding(theme::grid(1.0))
            .align_horizontal(UnitPoint::CENTER),
        Empty,
    );
    // Under the lines, not over them, so a click on it doesn't reach a line.
    Flex::column()
        .with_flex_child(
            Scroll::new(content.controller(FollowCurrentLine)).vertical(),
            1.0,
        )
        .with_child(back)
}

/// Stops scrolling to the line being sung once the user scrolls, until asked.
struct FollowCurrentLine;

impl<W: Widget<AppState>> Controller<AppState, W> for FollowCurrentLine {
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut AppState,
        env: &Env,
    ) {
        match event {
            // shortcut: dragging the scroll bar doesn't count, as the scroll
            // keeps those events; watch its offset if that's missed.
            Event::Wheel(_) => data.lyrics_scrolled = true,
            Event::Command(cmd) if cmd.is(FOLLOW_LYRICS) => data.lyrics_scrolled = false,
            Event::Notification(note) if note.is(SCROLL_TO_VIEW) && data.lyrics_scrolled => {
                // Keeps the scroll above from moving.
                ctx.set_handled();
                return;
            }
            _ => {}
        }
        child.event(ctx, event, data, env)
    }
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
                    let length =
                        utils::as_minutes_and_seconds(Duration::from_secs_f64(c.duration.max(0.0)));
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
        data.lyrics
            .resolve((), crate::webapi::lyrics_from_candidate(candidate));
        data.lyrics_search.results.clear();
    })
}

/// What a lyrics line needs to know to highlight itself.
#[derive(Clone, Data)]
struct Playhead {
    progress: Duration,
    playing: bool,
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
            playing: data.playback.state == PlaybackState::Playing,
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

/// Moves the playhead on between the player's reports, about one a second, so
/// each line lights up right as it's sung.
struct FollowSinging<W> {
    child: W,
    /// The lyrics as the lines get them, the playhead moved on.
    shown: Option<Ctx<Playhead, Lyrics>>,
    /// The last progress reported, and when.
    reported: (Duration, Instant),
    /// Wakes up at the next line's start or end.
    timer: TimerToken,
}

impl<W> FollowSinging<W> {
    fn new(child: W) -> Self {
        Self {
            child,
            shown: None,
            reported: (Duration::ZERO, Instant::now()),
            timer: TimerToken::INVALID,
        }
    }
}

/// How long until a line starts or ends, if the lyrics are synced and playing.
fn next_line_change(lyrics: &Ctx<Playhead, Lyrics>) -> Option<Duration> {
    if !(lyrics.ctx.synced && lyrics.ctx.playing) {
        return None;
    }
    let at = (lyrics.ctx.progress + HIGHLIGHT_LOOKAHEAD).as_millis() as u64;
    let next = (lyrics.data.lines.iter())
        .flat_map(|line| [line.start_ms(), line.end_ms()])
        // The last line ends at u64::MAX, never.
        .filter(|&ms| ms > at && ms != u64::MAX)
        .min()?;
    Some(Duration::from_millis(next - at))
}

impl<W: Widget<Ctx<Playhead, Lyrics>>> Widget<Ctx<Playhead, Lyrics>> for FollowSinging<W> {
    fn event(
        &mut self,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut Ctx<Playhead, Lyrics>,
        env: &Env,
    ) {
        if matches!(event, Event::Timer(token) if *token == self.timer) {
            ctx.request_update();
            return;
        }
        // The lines only read it.
        let mut shown = self.shown.clone().unwrap_or_else(|| data.clone());
        self.child.event(ctx, event, &mut shown, env);
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &Ctx<Playhead, Lyrics>,
        env: &Env,
    ) {
        if let LifeCycle::WidgetAdded = event {
            self.reported = (data.ctx.progress, Instant::now());
            self.shown = Some(data.clone());
        }
        let shown = self.shown.as_ref().unwrap_or(data);
        self.child.lifecycle(ctx, event, shown, env);
    }

    fn update(
        &mut self,
        ctx: &mut UpdateCtx,
        old_data: &Ctx<Playhead, Lyrics>,
        data: &Ctx<Playhead, Lyrics>,
        env: &Env,
    ) {
        let (old, new) = (&old_data.ctx, &data.ctx);
        if old.progress != new.progress || old.playing != new.playing {
            self.reported = (new.progress, Instant::now());
        }
        let mut shown = data.clone();
        if new.playing {
            shown.ctx.progress = self.reported.0 + self.reported.1.elapsed();
        }
        let old_shown = self.shown.as_ref().unwrap_or(old_data);
        self.child.update(ctx, old_shown, &shown, env);
        self.timer = match next_line_change(&shown) {
            Some(wait) => ctx.request_timer(wait),
            None => TimerToken::INVALID,
        };
        self.shown = Some(shown);
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &Ctx<Playhead, Lyrics>,
        env: &Env,
    ) -> Size {
        let shown = self.shown.as_ref().unwrap_or(data);
        self.child.layout(ctx, bc, shown, env)
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &Ctx<Playhead, Lyrics>, env: &Env) {
        self.child
            .paint(ctx, self.shown.as_ref().unwrap_or(data), env)
    }
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
                std::cmp::Ordering::Greater => env.set(theme::TEXT_COLOR, env.get(theme::GREY_400)),
                std::cmp::Ordering::Equal => {}
            }
        })
        .on_update(move |ctx, old, new, _| {
            if is_current_line(new) && !is_current_line(old) {
                // Keep some lines of context around the current one.
                ctx.scroll_area_to_view(ctx.size().to_rect().inflate(0.0, style.scroll_margin));
            }
        })
        .on_command(FOLLOW_LYRICS, move |ctx, _, line| {
            if is_current_line(line) {
                ctx.scroll_area_to_view(ctx.size().to_rect().inflate(0.0, style.scroll_margin));
            }
        })
        .on_left_click(|ctx, _, line, _| {
            let start = line.data.start_ms();
            if start != 0 {
                ctx.submit_command(cmd::SKIP_TO_POSITION.with(start));
                ctx.submit_command(FOLLOW_LYRICS);
            }
        })
}

fn lyrics_lines_widget(style: LineStyle) -> impl Widget<AppState> {
    Async::new(
        utils::spinner_widget,
        move || {
            let lines = List::new(move || lyrics_line_widget(style)).lens(Ctx::map(Lyrics::lines));
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(FollowSinging::new(lines))
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
            data.lyrics_scrolled = false;
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
/// The lyrics under the mini player's controls.
pub fn mini_player_lyrics() -> impl Widget<AppState> {
    panel_lines()
}

/// The lyrics in small print, for the panel and the mini player.
fn panel_lines() -> impl Widget<AppState> {
    lyrics_scroll(lyrics_lines_widget(PANEL_STYLE).padding(Insets::new(
        theme::grid(0.5),
        0.0,
        theme::grid(0.5),
        theme::grid(1.5),
    )))
    .expand()
}

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
    let panel = Flex::column()
        .with_child(header)
        .with_flex_child(panel_lines(), 1.0)
        .fix_size(theme::grid(42.0), theme::grid(30.0))
        .background(theme::BACKGROUND_DARK)
        .rounded(theme::GRID)
        .border(theme::GREY_500, 1.0);
    Either::new(|data: &AppState, _| data.lyrics_panel_open, panel, Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_up_when_the_next_line_starts() {
        let line = |start: u64| TrackLines {
            start_time_ms: start.to_string(),
            words: "la".to_string(),
            end_time_ms: "0".to_string(),
        };
        let lyrics = Lyrics::new([line(1000), line(3000)], "test");
        let at = |ms: u64, playing| {
            let progress = Duration::from_millis(ms) - HIGHLIGHT_LOOKAHEAD;
            let playhead = Playhead {
                progress,
                playing,
                synced: true,
            };
            next_line_change(&Ctx::new(playhead, lyrics.clone()))
        };
        assert_eq!(at(400, true), Some(Duration::from_millis(600)));
        assert_eq!(at(1000, true), Some(Duration::from_millis(2000)));
        // The last line lasts to the end.
        assert_eq!(at(3500, true), None);
        assert_eq!(at(400, false), None);
    }
}
