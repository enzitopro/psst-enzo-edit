use std::time::Duration;

use crate::{data::AppState, ui::theme};
use druid::{widget::prelude::*, TimerToken};

/// How often the "Same as Windows" theme checks the Windows setting.
const SYSTEM_THEME_CHECK: Duration = Duration::from_secs(5);

pub struct ThemeScope<W> {
    inner: W,
    cached_env: Option<Env>,
    outer_env: Option<Env>,
    dark: bool,
    timer: TimerToken,
}

impl<W> ThemeScope<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            cached_env: None,
            outer_env: None,
            dark: false,
            timer: TimerToken::INVALID,
        }
    }

    fn set_env(&mut self, data: &AppState, outer_env: &Env) {
        let mut themed_env = outer_env.clone();
        theme::setup(&mut themed_env, data);
        self.cached_env.replace(themed_env);
        self.outer_env.replace(outer_env.clone());
        self.dark = theme::is_dark(&data.config);
    }
}

impl<W: Widget<AppState>> Widget<AppState> for ThemeScope<W> {
    fn event(&mut self, ctx: &mut EventCtx, event: &Event, data: &mut AppState, env: &Env) {
        if let Event::Timer(token) = event {
            if *token == self.timer {
                self.timer = ctx.request_timer(SYSTEM_THEME_CHECK);
                let follows = data.config.theme_follows_windows;
                if follows && theme::is_dark(&data.config) != self.dark {
                    let outer_env = self.outer_env.clone().unwrap_or_else(|| env.clone());
                    self.set_env(data, &outer_env);
                    // The children pick up the new env in an update pass.
                    ctx.request_update();
                    ctx.request_layout();
                    ctx.request_paint();
                }
                return;
            }
        }
        self.inner
            .event(ctx, event, data, self.cached_env.as_ref().unwrap_or(env))
    }

    fn lifecycle(&mut self, ctx: &mut LifeCycleCtx, event: &LifeCycle, data: &AppState, env: &Env) {
        if let LifeCycle::WidgetAdded = &event {
            self.set_env(data, env);
            self.timer = ctx.request_timer(SYSTEM_THEME_CHECK);
        }
        self.inner
            .lifecycle(ctx, event, data, self.cached_env.as_ref().unwrap_or(env))
    }

    fn update(&mut self, ctx: &mut UpdateCtx, old_data: &AppState, data: &AppState, env: &Env) {
        if !data.config.theme.same(&old_data.config.theme)
            || data.config.theme_follows_windows != old_data.config.theme_follows_windows
        {
            self.set_env(data, env);
            ctx.request_layout();
            ctx.request_paint();
        }
        self.inner
            .update(ctx, old_data, data, self.cached_env.as_ref().unwrap_or(env));
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &AppState,
        env: &Env,
    ) -> Size {
        self.inner
            .layout(ctx, bc, data, self.cached_env.as_ref().unwrap_or(env))
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &AppState, env: &Env) {
        self.inner
            .paint(ctx, data, self.cached_env.as_ref().unwrap_or(env));
    }
}
