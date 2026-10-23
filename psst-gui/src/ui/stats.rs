use std::sync::Arc;

use druid::{
    widget::{CrossAxisAlignment, Flex, Label, LineBreaking, List},
    Insets, Selector, Widget, WidgetExt,
};
use psst_core::lastfm;

use crate::{
    cmd,
    data::{AppState, Config, Nav, StatEntry, Stats},
    error::Error,
    widget::{Async, MyWidgetExt},
};

use super::{theme, utils};

/// Loads the top of a period, "7day", "1month", "12month" or "overall".
pub const LOAD_STATS: Selector<Arc<str>> = Selector::new("app.stats.load");

const PERIODS: [(&str, &str); 4] = [
    ("Week", "7day"),
    ("Month", "1month"),
    ("Year", "12month"),
    ("All Time", "overall"),
];

/// Your Last.fm top artists and tracks of a period; each opens a search for it.
pub fn stats_widget() -> impl Widget<AppState> {
    let mut periods = Flex::row();
    for (label, period) in PERIODS {
        periods.add_child(
            Label::new(label)
                .with_text_size(theme::TEXT_SIZE_SMALL)
                .env_scope(move |env, data: &AppState| {
                    if &*data.stats_period != period {
                        env.set(theme::TEXT_COLOR, env.get(theme::PLACEHOLDER_COLOR));
                    }
                })
                .padding(Insets::uniform_xy(theme::grid(1.0), theme::grid(0.5)))
                .link()
                .rounded(theme::BUTTON_BORDER_RADIUS)
                .on_left_click(move |ctx, _, data: &mut AppState, _| {
                    data.stats_period = period.into();
                    ctx.submit_command(LOAD_STATS.with(period.into()));
                }),
        );
    }
    let stats = Async::new(utils::spinner_widget, loaded_widget, utils::error_widget)
        .lens(AppState::stats)
        .on_command_async(
            LOAD_STATS,
            |period| load(&period),
            |_, data: &mut AppState, period| data.stats.defer(period),
            |_, data, result| data.stats.update(result),
        );
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(periods)
        .with_spacer(theme::grid(1.0))
        .with_child(stats)
}

/// No app state off the UI thread, but the Last.fm keys are saved when set.
fn load(period: &str) -> Result<Stats, Error> {
    let config = Config::load().unwrap_or_default();
    let (Some(key), Some(secret), Some(session)) = (
        config.lastfm_api_key,
        config.lastfm_api_secret,
        config.lastfm_session_key,
    ) else {
        return Err(Error::WebApiError(
            "Connect Last.fm in Preferences to see your stats".into(),
        ));
    };
    let top = lastfm::top_stats(&key, &secret, &session, period)
        .map_err(|err| Error::WebApiError(format!("Last.fm: {err}")))?;
    Ok(Stats {
        scrobbles: top.scrobbles,
        artists: top
            .artists
            .into_iter()
            .map(|(name, plays)| StatEntry {
                query: name.as_str().into(),
                title: name.into(),
                subtitle: "".into(),
                plays,
            })
            .collect(),
        tracks: top
            .tracks
            .into_iter()
            .map(|(artist, title, plays)| StatEntry {
                query: format!("{title} {artist}").into(),
                title: title.into(),
                subtitle: artist.into(),
                plays,
            })
            .collect(),
    })
}

fn loaded_widget() -> impl Widget<Stats> {
    let section = |title: &str| {
        Label::new(title)
            .with_font(theme::UI_FONT_MEDIUM)
            .with_text_color(theme::PLACEHOLDER_COLOR)
            .with_text_size(theme::TEXT_SIZE_SMALL)
            .padding((0.0, theme::grid(2.0), 0.0, theme::grid(1.0)))
    };
    Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(
            Label::dynamic(|stats: &Stats, _| {
                format!(
                    "{} scrobbles in total",
                    utils::format_number_with_commas(stats.scrobbles as i64)
                )
            })
            .with_text_size(theme::TEXT_SIZE_SMALL),
        )
        .with_child(section("Top Artists"))
        .with_child(List::new(entry_widget).lens(Stats::artists))
        .with_child(section("Top Tracks"))
        .with_child(List::new(entry_widget).lens(Stats::tracks))
}

fn entry_widget() -> impl Widget<StatEntry> {
    let title = Label::raw()
        .with_font(theme::UI_FONT_MEDIUM)
        .with_line_break_mode(LineBreaking::Clip)
        .lens(StatEntry::title);
    let subtitle = Label::raw()
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .lens(StatEntry::subtitle);
    let plays = Label::dynamic(|entry: &StatEntry, _| match entry.plays {
        1 => "1 play".to_string(),
        plays => format!("{} plays", utils::format_number_with_commas(plays as i64)),
    })
    .with_text_size(theme::TEXT_SIZE_SMALL)
    .with_text_color(theme::PLACEHOLDER_COLOR);
    Flex::row()
        .with_flex_child(
            Flex::column()
                .cross_axis_alignment(CrossAxisAlignment::Start)
                .with_child(title)
                .with_child(subtitle),
            1.0,
        )
        .with_child(plays)
        .padding(theme::grid(1.0))
        .expand_width()
        .link()
        .rounded(theme::BUTTON_BORDER_RADIUS)
        .on_left_click(|ctx, _, entry: &mut StatEntry, _| {
            ctx.submit_command(cmd::NAVIGATE.with(Nav::SearchResults(entry.query.clone())));
        })
}
