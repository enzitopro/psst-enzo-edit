use std::{collections::BTreeMap, marker::PhantomData, mem, ops::Range, sync::Arc};

use druid::{
    im::Vector,
    kurbo::Line,
    lens::Map,
    piet::StrokeStyle,
    widget::{prelude::*, Controller, ControllerHost, Flex, Painter, ViewSwitcher},
    Data, Lens, Point, Rect, Selector, WidgetExt, WidgetPod,
};

use crate::{
    cmd,
    data::{
        AppState, CommonCtx, FindQuery, MatchFindQuery, Playable, PlaybackOrigin, PlaybackPayload,
        PlaylistTracks, QueueBehavior, QueueEntry, SavedTracks, SearchResults, ShowEpisodes, Track,
        WithCtx,
    },
    ui::theme,
    widget::{icons, MyWidgetExt},
};

use super::{
    episode,
    find::{Find, MATCHES},
    playback, track,
};

#[derive(Copy, Clone)]
pub struct Display {
    pub track: track::Display,
}

pub fn list_widget<T>(display: Display) -> impl Widget<WithCtx<T>>
where
    T: PlayableIter + Data,
{
    ControllerHost::new(
        PlayableList::new(move || playable_widget(display), None),
        PlayController::default(),
    )
}

pub fn list_widget_with_find<T>(
    display: Display,
    selector: Selector<Find>,
) -> impl Widget<WithCtx<T>>
where
    T: PlayableIter + Data,
{
    ControllerHost::new(
        PlayableList::new(move || playable_widget(display), Some(selector)),
        PlayController::default(),
    )
}

type Row = WidgetPod<PlayRow<Playable>, Box<dyn Widget<PlayRow<Playable>>>>;

/// The rows of a list that are in view, plus a margin: the others aren't built.
/// A list of 4000 liked songs built 4000 rows, ~170 MB, and laid them all out.
/// The rows must all be as tall, the list measures the first one.
pub struct PlayableList<T> {
    make_row: Box<dyn Fn() -> Box<dyn Widget<PlayRow<Playable>>>>,
    /// The built rows, by position.
    rows: BTreeMap<usize, Row>,
    row_height: f64,
    /// The part of the list in view, as Druid tells when it scrolls.
    visible: Rect,
    /// The list answers the finder (Ctrl+F) and highlights what matches.
    find: Option<Selector<Find>>,
    query: Option<FindQuery>,
    list: PhantomData<T>,
}

/// Builds the rows that came into view, sent to the list itself.
const BUILD_ROWS: Selector = Selector::new("app.playable-list.build-rows");

/// Rows built past the ones in view, each way.
const MARGIN: usize = 10;

impl<T: PlayableIter + Data> PlayableList<T> {
    pub fn new<W: Widget<PlayRow<Playable>> + 'static>(
        make_row: impl Fn() -> W + 'static,
        find: Option<Selector<Find>>,
    ) -> Self {
        Self {
            make_row: Box::new(move || make_row().boxed()),
            rows: BTreeMap::new(),
            // Guesses until the first row is measured, and a tall window.
            row_height: theme::grid(6.0),
            visible: Rect::new(0.0, 0.0, 0.0, 1600.0),
            find,
            query: None,
            list: PhantomData,
        }
    }

    fn row(data: &WithCtx<T>, position: usize) -> Option<PlayRow<Playable>> {
        let item = data.data.get(position)?;
        let origin = Arc::new(data.data.origin());
        Some(PlayRow {
            is_playing: data.ctx.is_playing(&item, &origin),
            ctx: data.ctx.clone(),
            origin,
            item,
            position,
        })
    }

    /// The positions of the rows in view, and the margin.
    fn wanted(&self, count: usize) -> Range<usize> {
        let first = (self.visible.y0 / self.row_height).floor().max(0.0) as usize;
        let last = (self.visible.y1 / self.row_height).ceil().max(0.0) as usize;
        first.saturating_sub(MARGIN)..(last + MARGIN).min(count)
    }

    /// Builds the rows coming into view and drops the ones far from it.
    /// Whether any did.
    fn sync_rows(&mut self, count: usize) -> bool {
        let wanted = self.wanted(count);
        let keep = wanted.start.saturating_sub(MARGIN)..wanted.end + MARGIN;
        let before = self.rows.len();
        self.rows
            .retain(|position, _| keep.contains(position) && *position < count);
        let mut changed = self.rows.len() != before;
        for position in wanted {
            if !self.rows.contains_key(&position) {
                self.rows
                    .insert(position, WidgetPod::new((self.make_row)()));
                changed = true;
            }
        }
        changed
    }

    fn row_rect(&self, position: usize, width: f64) -> Rect {
        let top = position as f64 * self.row_height;
        Rect::new(0.0, top, width, top + self.row_height)
    }

    /// Counts the matches for the finder and brings the focused one into view.
    fn find(&mut self, ctx: &mut EventCtx, find: &Find, data: &WithCtx<T>) {
        let matches: Vec<_> = (0..data.data.count())
            .filter(|&position| {
                Self::row(data, position).is_some_and(|row| row.matches_query(&find.query))
            })
            .collect();
        ctx.submit_command(MATCHES.with(matches.len()).to(find.sender));
        if let Some(&focused) = matches.get(find.focus.min(matches.len().saturating_sub(1))) {
            ctx.scroll_area_to_view(self.row_rect(focused, ctx.size().width));
        }
        self.query = (!find.query.is_empty()).then(|| find.query.clone());
        ctx.request_paint();
    }
}

impl<T: PlayableIter + Data> Widget<WithCtx<T>> for PlayableList<T> {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut WithCtx<T>, env: &Env) {
        match event {
            Event::Command(cmd) if self.find.is_some_and(|selector| cmd.is(selector)) => {
                let find = cmd.get_unchecked(self.find.unwrap()).clone();
                self.find(ctx, &find, data);
            }
            Event::Command(cmd) if cmd.is(BUILD_ROWS) => {
                if self.sync_rows(data.data.count()) {
                    ctx.children_changed();
                }
                ctx.set_handled();
            }
            _ => {
                for (&position, row) in self.rows.iter_mut() {
                    if let (true, Some(mut item)) =
                        (row.is_initialized(), Self::row(data, position))
                    {
                        row.event(ctx, event, &mut item, env);
                    }
                }
            }
        }
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &WithCtx<T>,
        env: &Env,
    ) {
        match event {
            LifeCycle::WidgetAdded => {
                self.sync_rows(data.data.count());
                ctx.children_changed();
            }
            LifeCycle::ViewContextChanged(view) => {
                self.visible = view.clip;
                if self.sync_rows(data.data.count()) {
                    ctx.children_changed();
                }
            }
            _ => {}
        }
        // New rows only get the event that adds them.
        let adding = matches!(event, LifeCycle::WidgetAdded | LifeCycle::Internal(_));
        for (&position, row) in self.rows.iter_mut() {
            if let (true, Some(item)) = (adding || row.is_initialized(), Self::row(data, position))
            {
                row.lifecycle(ctx, event, &item, env);
            }
        }
    }

    fn update(&mut self, ctx: &mut UpdateCtx, old_data: &WithCtx<T>, data: &WithCtx<T>, env: &Env) {
        for (&position, row) in self.rows.iter_mut() {
            if let (true, Some(item)) = (row.is_initialized(), Self::row(data, position)) {
                row.update(ctx, &item, env);
            }
        }
        let count = data.data.count();
        if count != old_data.data.count() {
            ctx.request_layout();
        }
        if self.sync_rows(count) {
            ctx.children_changed();
        }
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &WithCtx<T>,
        env: &Env,
    ) -> Size {
        let row_bc = BoxConstraints::new(
            Size::new(bc.min().width, 0.0),
            Size::new(bc.max().width, f64::INFINITY),
        );
        let mut width = bc.min().width;
        let mut paint_rect = Rect::ZERO;
        let mut measured = false;
        for (&position, row) in self.rows.iter_mut() {
            let Some(item) = Self::row(data, position) else {
                continue;
            };
            if !row.is_initialized() {
                continue;
            }
            let size = row.layout(ctx, &row_bc, &item, env);
            if !measured && size.height > 0.0 {
                self.row_height = size.height;
                measured = true;
            }
            width = width.max(size.width);
            row.set_origin(ctx, Point::new(0.0, position as f64 * self.row_height));
            paint_rect = paint_rect.union(row.paint_rect());
        }
        let count = data.data.count();
        // Measured now, the rows in view may be others than the guess built.
        let wanted = self.wanted(count);
        if wanted
            .clone()
            .any(|position| !self.rows.contains_key(&position))
        {
            ctx.submit_command(BUILD_ROWS.to(ctx.widget_id()));
        }
        let size = bc.constrain(Size::new(width, count as f64 * self.row_height));
        ctx.set_paint_insets(paint_rect - size.to_rect());
        size
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &WithCtx<T>, env: &Env) {
        for (&position, row) in self.rows.iter_mut() {
            let Some(item) = Self::row(data, position) else {
                continue;
            };
            if !row.is_initialized() {
                continue;
            }
            if self
                .query
                .as_ref()
                .is_some_and(|query| item.matches_query(query))
            {
                let bounds = row
                    .layout_rect()
                    .inset(-2.0)
                    .to_rounded_rect(env.get(theme::BUTTON_BORDER_RADIUS));
                ctx.fill(bounds, &env.get(theme::GREY_500));
            }
            row.paint(ctx, &item, env);
        }
    }
}

pub fn playable_widget(display: Display) -> impl Widget<PlayRow<Playable>> {
    // The `List` reuses row widgets by index, so when the tracks are sorted a
    // row gets another track.  The lenses must read the row's current item,
    // the one captured at construction is only a fallback for the moment the
    // switcher hasn't rebuilt a row that changed kind yet.
    ViewSwitcher::new(
        |row: &PlayRow<Playable>, _| mem::discriminant(&row.item),
        move |_, row: &PlayRow<Playable>, _| match row.item.clone() {
            Playable::Track(fallback) => track::playable_widget(display.track)
                .lens(Map::new(
                    move |pb: &PlayRow<Playable>| match &pb.item {
                        Playable::Track(track) => pb.with(track.clone()),
                        Playable::Episode(_) => pb.with(fallback.clone()),
                    },
                    |_, _| {
                        // Ignore mutation.
                    },
                ))
                .boxed(),
            Playable::Episode(fallback) => episode::playable_widget()
                .lens(Map::new(
                    move |pb: &PlayRow<Playable>| match &pb.item {
                        Playable::Episode(episode) => pb.with(episode.clone()),
                        Playable::Track(_) => pb.with(fallback.clone()),
                    },
                    |_, _| {
                        // Ignore mutation.
                    },
                ))
                .boxed(),
        },
    )
}

pub fn is_playing_marker_widget() -> impl Widget<bool> {
    Painter::new(|ctx, is_playing, env| {
        const STYLE: StrokeStyle = StrokeStyle::new().dash_pattern(&[1.0, 2.0]);

        let y = ctx.size().height / 2.0;
        let line = Line::new((0.0, y), (ctx.size().width, y));
        let color = if *is_playing {
            env.get(theme::GREY_300)
        } else {
            env.get(theme::GREY_500)
        };
        ctx.stroke_styled(line, &color, 1.0, &STYLE);
    })
}

#[derive(Clone, Data, Lens)]
pub struct PlayRow<T> {
    pub item: T,
    pub ctx: Arc<CommonCtx>,
    pub origin: Arc<PlaybackOrigin>,
    pub position: usize,
    pub is_playing: bool,
}

impl<T> PlayRow<T> {
    fn with<U>(&self, item: U) -> PlayRow<U> {
        PlayRow {
            item,
            ctx: self.ctx.clone(),
            origin: self.origin.clone(),
            position: self.position,
            is_playing: self.is_playing,
        }
    }
}

impl MatchFindQuery for PlayRow<Playable> {
    fn matches_query(&self, q: &FindQuery) -> bool {
        match &self.item {
            Playable::Track(track) => {
                q.matches_str(&track.name)
                    || track.album.iter().any(|a| q.matches_str(&a.name))
                    || track.artists.iter().any(|a| q.matches_str(&a.name))
            }
            Playable::Episode(episode) => {
                q.matches_str(&episode.name)
                    || q.matches_str(&episode.description)
                    || q.matches_str(&episode.show.name)
            }
        }
    }
}

pub trait PlayableIter {
    fn origin(&self) -> PlaybackOrigin;
    fn count(&self) -> usize;
    fn get(&self, index: usize) -> Option<Playable>;

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for index in 0..self.count() {
            if let Some(item) = self.get(index) {
                cb(item, index);
            }
        }
    }
}

// This should change to a more specific name as it could be confusing for others
// As at the moment this is only used for the home page!
impl PlayableIter for Vector<Arc<Track>> {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Home
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.get(index).map(|track| Playable::Track(track.clone()))
    }

    fn count(&self) -> usize {
        self.len()
    }
}

/// The queue's items.  Played again from the queue, they count as from Home.
impl PlayableIter for Vector<QueueEntry> {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Home
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.get(index).map(|entry| entry.item.clone())
    }

    fn count(&self) -> usize {
        self.len()
    }
}

impl PlayableIter for PlaylistTracks {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Playlist(self.link())
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.tracks
            .get(index)
            .map(|track| Playable::Track(track.clone()))
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for SavedTracks {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Library
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.tracks
            .get(index)
            .map(|track| Playable::Track(track.clone()))
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for SearchResults {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Search(self.query.clone())
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.tracks
            .get(index)
            .map(|track| Playable::Track(track.clone()))
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for ShowEpisodes {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Show(self.show.clone())
    }

    fn get(&self, index: usize) -> Option<Playable> {
        self.episodes
            .get(index)
            .map(|episode| Playable::Episode(episode.clone()))
    }

    fn count(&self) -> usize {
        self.episodes.len()
    }
}

/// Plays `data` from the item at `clicked`.  Local items without a file are left
/// out, and the position shifts to account for the ones skipped before it.
pub fn payload(data: &impl PlayableIter, clicked: usize) -> Option<PlaybackPayload> {
    let mut items = Vector::new();
    let mut position = None;
    data.for_each(|item, index| {
        if item.is_playable() {
            if position.is_none() && index >= clicked {
                position = Some(items.len());
            }
            items.push_back(item);
        }
    });
    (!items.is_empty()).then(|| PlaybackPayload {
        items,
        origin: data.origin(),
        position: position.unwrap_or(0),
    })
}

/// Play, and the play mode toggle (shuffle, repeat...) it follows: from the top,
/// or from a random track in shuffle mode.  `tracks` reads the page's list.
pub fn play_buttons<T: PlayableIter + 'static>(
    tracks: fn(&AppState) -> Option<&T>,
) -> impl Widget<AppState> {
    Flex::row()
        .with_child(
            icons::PLAY
                .scale((theme::grid(3.0), theme::grid(3.0)))
                .padding(theme::grid(1.0))
                .link()
                .circle()
                .border(theme::GREY_500, 1.0)
                .on_left_click(move |ctx, _, data: &mut AppState, _| {
                    let Some(tracks) = tracks(data) else {
                        return;
                    };
                    let start = match data.playback.queue_behavior {
                        QueueBehavior::Random if tracks.count() > 0 => {
                            rand::random_range(0..tracks.count())
                        }
                        _ => 0,
                    };
                    if let Some(payload) = payload(tracks, start) {
                        ctx.submit_command(cmd::PLAY_TRACKS.with(payload));
                    }
                }),
        )
        .with_default_spacer()
        .with_child(playback::queue_behavior_widget().lens(AppState::playback))
}

/// Selects the track at `clicked` (Ctrl+click), or the ones from `anchor`, the
/// last Ctrl+click, to it (Shift+click, `range`).
pub fn select(
    ctx: &mut EventCtx,
    list: &impl PlayableIter,
    anchor: &mut Option<usize>,
    clicked: usize,
    range: bool,
) {
    let from = if range {
        anchor.unwrap_or(clicked)
    } else {
        *anchor = Some(clicked);
        clicked
    };
    let rows = from.min(clicked)..=from.max(clicked);
    let mut tracks = Vec::new();
    list.for_each(|item, position| {
        if let (true, Playable::Track(track)) = (rows.contains(&position), item) {
            tracks.push(track);
        }
    });
    ctx.submit_command(cmd::SELECT_TRACKS.with((tracks, range)));
}

#[derive(Default)]
struct PlayController {
    /// Where a Shift+click range starts: the last Ctrl+click.
    anchor: Option<usize>,
}

impl<T, W> Controller<WithCtx<T>, W> for PlayController
where
    T: PlayableIter + Data,
    W: Widget<WithCtx<T>>,
{
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut WithCtx<T>,
        env: &Env,
    ) {
        match event {
            Event::Notification(note) => {
                if let Some(&clicked) = note.get(cmd::PLAY) {
                    if let Some(payload) = payload(&data.data, clicked) {
                        ctx.submit_command(cmd::PLAY_TRACKS.with(payload));
                    }
                    ctx.set_handled();
                }
                if let Some(&(clicked, range)) = note.get(cmd::SELECT) {
                    select(ctx, &data.data, &mut self.anchor, clicked, range);
                    ctx.set_handled();
                }
            }
            _ => child.event(ctx, event, data, env),
        }
    }
}

#[cfg(test)]
mod tests {
    use druid::widget::SizedBox;

    use super::*;

    #[test]
    fn builds_the_rows_in_view_and_a_margin() {
        let mut list = PlayableList::<Vector<Arc<Track>>>::new(SizedBox::empty, None);
        list.row_height = 60.0;
        list.visible = Rect::new(0.0, 0.0, 800.0, 600.0);
        assert!(list.sync_rows(4000));
        let built = |list: &PlayableList<_>| list.rows.keys().copied().collect::<Vec<_>>();
        assert_eq!(built(&list), (0..20).collect::<Vec<_>>());
        // Scrolled far down: the rows up top go, the ones in view come.
        list.visible = Rect::new(0.0, 6000.0, 800.0, 6600.0);
        assert!(list.sync_rows(4000));
        assert_eq!(built(&list), (90..120).collect::<Vec<_>>());
        // A little further: the ones just left behind stay.
        list.visible = Rect::new(0.0, 6120.0, 800.0, 6720.0);
        list.sync_rows(4000);
        assert_eq!(built(&list), (90..122).collect::<Vec<_>>());
        // A shorter list drops what's past its end.
        list.sync_rows(100);
        assert_eq!(built(&list), (90..100).collect::<Vec<_>>());
        assert!(!list.sync_rows(100));
    }
}
