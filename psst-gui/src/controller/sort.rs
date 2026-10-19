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
        let criteria = match event {
            Event::Command(cmd) if cmd.is(cmd::TOGGLE_SORT_ORDER) => {
                data.config.sort_order = match data.config.sort_order {
                    SortOrder::Ascending => SortOrder::Descending,
                    SortOrder::Descending => SortOrder::Ascending,
                };
                data.resort_tracks();
                ctx.set_handled();
                return;
            }
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_TITLE) => SortCriteria::Title,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_ALBUM) => SortCriteria::Album,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_DATE_ADDED) => SortCriteria::DateAdded,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_ARTIST) => SortCriteria::Artist,
            Event::Command(cmd) if cmd.is(cmd::SORT_BY_DURATION) => SortCriteria::Duration,
            _ => {
                child.event(ctx, event, data, env);
                return;
            }
        };
        if data.config.sort_criteria != criteria {
            data.config.sort_criteria = criteria;
            // Sorted in place, the tracks are already loaded.
            data.resort_tracks();
        }
        ctx.set_handled();
    }
}
