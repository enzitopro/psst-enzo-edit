use druid::{
    im::Vector,
    widget::{Controller, CrossAxisAlignment, Either, Flex, Label, List, Scroll},
    Env, Event, EventCtx, Lens, LensExt, Widget, WidgetExt,
};

use crate::{
    cmd,
    data::{AppState, Ctx, Playable, Playback, QueueEdit, QueueEntry, WithCtx},
    widget::{Empty, MyWidgetExt},
};

use super::{playable, playable::PlayRow, theme, track};

/// Only this many rows of each list are drawn: a queue of liked songs has
/// thousands.
const SHOWN: usize = 100;

const DISPLAY: playable::Display = playable::Display {
    track: track::Display {
        number: false,
        title: true,
        artist: true,
        album: false,
        cover: false,
        popularity: false,
    },
};

/// What plays next, to reorder, and what played before, next to the page.
/// Rows have the like button and the usual track menu.
pub fn panel_widget() -> impl Widget<AppState> {
    let upcoming = Either::new(
        |upcoming: &WithCtx<Vector<QueueEntry>>, _| upcoming.data.is_empty(),
        placeholder("Nothing plays next"),
        List::new(upcoming_row).controller(PlayNow),
    )
    .lens(shown(|playback| &playback.upcoming));
    let history = Either::new(
        |history: &WithCtx<Vector<QueueEntry>>, _| history.data.is_empty(),
        placeholder("Nothing played yet"),
        playable::list_widget(DISPLAY),
    )
    .lens(shown(|playback| &playback.history));
    let panel = Flex::column()
        .cross_axis_alignment(CrossAxisAlignment::Fill)
        .with_child(header("Next Up"))
        .with_child(upcoming)
        .with_child(header("Recently Played"))
        .with_child(history)
        .padding(theme::grid(1.0));
    Either::new(
        |data: &AppState, _| data.queue_panel_open,
        Scroll::new(panel)
            .vertical()
            .fix_width(theme::grid(40.0))
            .background(theme::BACKGROUND_DARK),
        Empty,
    )
    .on_command(cmd::TOGGLE_QUEUE, |_, _, data| {
        data.queue_panel_open = !data.queue_panel_open;
    })
}

/// The first rows of a queue list, read-only.
fn shown(
    list: fn(&Playback) -> &Vector<QueueEntry>,
) -> impl Lens<AppState, WithCtx<Vector<QueueEntry>>> {
    let rows = AppState::playback.map(
        move |playback: &Playback| list(playback).take(SHOWN),
        |_, _| {},
    );
    Ctx::make(AppState::common_ctx, rows)
}

fn upcoming_row() -> impl Widget<PlayRow<Playable>> {
    let edit = |label: &str, edit: fn(usize) -> QueueEdit| {
        Label::new(label)
            .padding(theme::grid(0.5))
            .link()
            .rounded(theme::BUTTON_BORDER_RADIUS)
            .on_left_click(move |ctx, _, row: &mut PlayRow<Playable>, _| {
                ctx.submit_command(cmd::EDIT_QUEUE.with(edit(row.position)));
            })
    };
    Flex::row()
        .with_flex_child(playable::playable_widget(DISPLAY), 1.0)
        .with_child(edit("↑", QueueEdit::MoveUp))
        .with_child(edit("↓", QueueEdit::MoveDown))
        .with_child(edit("✕", QueueEdit::Remove))
}

/// A click on an upcoming row plays it right away, without replacing the queue.
struct PlayNow;

impl<W: Widget<WithCtx<Vector<QueueEntry>>>> Controller<WithCtx<Vector<QueueEntry>>, W>
    for PlayNow
{
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut WithCtx<Vector<QueueEntry>>,
        env: &Env,
    ) {
        if let Event::Notification(note) = event {
            if let Some(&clicked) = note.get(cmd::PLAY) {
                ctx.submit_command(cmd::EDIT_QUEUE.with(QueueEdit::PlayNow(clicked)));
                ctx.set_handled();
                return;
            }
        }
        child.event(ctx, event, data, env);
    }
}

fn header<T: druid::Data>(text: &str) -> impl Widget<T> {
    Label::new(text)
        .with_font(theme::UI_FONT_MEDIUM)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .padding((theme::grid(1.0), theme::grid(2.0), 0.0, theme::grid(1.0)))
}

fn placeholder<T: druid::Data>(text: &str) -> impl Widget<T> {
    Label::new(text)
        .with_text_size(theme::TEXT_SIZE_SMALL)
        .with_text_color(theme::PLACEHOLDER_COLOR)
        .padding(theme::grid(1.0))
}
