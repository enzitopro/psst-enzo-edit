use std::{mem, sync::Arc};

use druid::{
    im::Vector,
    kurbo::Line,
    lens::Map,
    piet::StrokeStyle,
    widget::{Controller, ControllerHost, Flex, List, ListIter, Painter, ViewSwitcher},
    Data, Env, Event, EventCtx, Lens, RenderContext, Selector, Widget, WidgetExt,
};

use crate::{
    cmd,
    data::{
        AppState, CommonCtx, FindQuery, MatchFindQuery, Playable, PlaybackOrigin, PlaybackPayload,
        PlaylistTracks, QueueBehavior, SavedTracks, SearchResults, ShowEpisodes, Track, WithCtx,
    },
    ui::theme,
    widget::{icons, MyWidgetExt},
};

use super::{
    episode,
    find::{Find, Findable},
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
    ControllerHost::new(List::new(move || playable_widget(display)), PlayController)
}

pub fn list_widget_with_find<T>(
    display: Display,
    selector: Selector<Find>,
) -> impl Widget<WithCtx<T>>
where
    T: PlayableIter + Data,
{
    ControllerHost::new(
        List::new(move || Findable::new(playable_widget(display), selector)),
        PlayController,
    )
}

fn playable_widget(display: Display) -> impl Widget<PlayRow<Playable>> {
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
    fn for_each(&self, cb: impl FnMut(Playable, usize));
}

// This should change to a more specific name as it could be confusing for others
// As at the moment this is only used for the home page!
impl PlayableIter for Vector<Arc<Track>> {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Home
    }

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for (position, track) in self.iter().enumerate() {
            cb(Playable::Track(track.to_owned()), position);
        }
    }

    fn count(&self) -> usize {
        self.len()
    }
}

impl PlayableIter for PlaylistTracks {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Playlist(self.link())
    }

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for (position, track) in self.tracks.iter().enumerate() {
            cb(Playable::Track(track.to_owned()), position);
        }
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for SavedTracks {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Library
    }

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for (position, track) in self.tracks.iter().enumerate() {
            cb(Playable::Track(track.to_owned()), position);
        }
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for SearchResults {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Search(self.query.clone())
    }

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for (position, track) in self.tracks.iter().enumerate() {
            cb(Playable::Track(track.to_owned()), position);
        }
    }

    fn count(&self) -> usize {
        self.tracks.len()
    }
}

impl PlayableIter for ShowEpisodes {
    fn origin(&self) -> PlaybackOrigin {
        PlaybackOrigin::Show(self.show.clone())
    }

    fn for_each(&self, mut cb: impl FnMut(Playable, usize)) {
        for (position, episode) in self.episodes.iter().enumerate() {
            cb(Playable::Episode(episode.to_owned()), position);
        }
    }

    fn count(&self) -> usize {
        self.episodes.len()
    }
}

impl<T> ListIter<PlayRow<Playable>> for WithCtx<T>
where
    T: PlayableIter + Data,
{
    fn for_each(&self, mut cb: impl FnMut(&PlayRow<Playable>, usize)) {
        let origin = Arc::new(self.data.origin());
        self.data.for_each(|item, position| {
            cb(
                &PlayRow {
                    is_playing: self.ctx.is_playing(&item),
                    ctx: self.ctx.to_owned(),
                    origin: origin.clone(),
                    item,
                    position,
                },
                position,
            )
        });
    }

    fn for_each_mut(&mut self, mut cb: impl FnMut(&mut PlayRow<Playable>, usize)) {
        let origin = Arc::new(self.data.origin());
        self.data.for_each(|item, position| {
            cb(
                &mut PlayRow {
                    is_playing: self.ctx.is_playing(&item),
                    ctx: self.ctx.to_owned(),
                    origin: origin.clone(),
                    item,
                    position,
                },
                position,
            )
        });
    }

    fn data_len(&self) -> usize {
        self.data.count()
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

struct PlayController;

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
            }
            _ => child.event(ctx, event, data, env),
        }
    }
}
