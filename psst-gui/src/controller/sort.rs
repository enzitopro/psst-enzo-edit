use druid::widget::{prelude::*, Controller};
use druid::{Event, EventCtx, Widget};

use crate::cmd;
use crate::data::config::SortCriteria;
use crate::data::{config::SortOrder, AppState};

pub struct SortController;

impl<W> Controller<AppState, W> for SortController
where
    W: Widget<AppState>,
{
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut AppState,
        env: &Env,
    ) {
        let (mut criteria, mut order) = data.current_sort();
        match event {
            Event::Command(cmd) if cmd.is(cmd::TOGGLE_SORT_ORDER) => {
                order = match order {
                    SortOrder::Ascending => SortOrder::Descending,
                    SortOrder::Descending => SortOrder::Ascending,
                };
            }
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_TITLE) => criteria = SortCriteria::Title,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_ALBUM) => criteria = SortCriteria::Album,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_DATE_ADDED) => {
                criteria = SortCriteria::DateAdded
            }
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_ARTIST) => criteria = SortCriteria::Artist,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_DURATION) => {
                criteria = SortCriteria::Duration
            }
            _ => {
                child.event(ctx, event, data, env);
                return;
            }
        }
        ctx.set_handled();
        if let Some(list) = data.nav.track_list().map(String::from) {
            if (criteria, order) != data.current_sort() {
                data.config.track_sorts.insert(list, (criteria, order));
                // Sorted in place, the tracks are already loaded.
                data.resort_tracks();
            }
        }
    }
}
