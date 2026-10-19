use druid::{widget::Controller, Env, Event, EventCtx, LifeCycle, LifeCycleCtx, Widget};

use crate::{
    cmd,
    data::{AppState, Nav},
    ui::playlist,
    webapi::{spotify_local_file_paths, LocalLibrary},
};

/// Keeps the local files index up to date: scans once the main window opens,
/// and again on every `RESCAN_LOCAL_FILES` (e.g. after the folders change in
/// the preferences).
pub struct LocalFilesController {
    scans_pending: usize,
}

impl LocalFilesController {
    const SCAN_FINISHED: druid::Selector<usize> =
        druid::Selector::new("app.local-files.scan-finished");

    pub fn new() -> Self {
        Self { scans_pending: 0 }
    }
}

impl<W: Widget<AppState>> Controller<AppState, W> for LocalFilesController {
    fn event(
        &mut self,
        child: &mut W,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut AppState,
        env: &Env,
    ) {
        match event {
            Event::Command(cmd) if cmd.is(cmd::RESCAN_LOCAL_FILES) => {
                let request = cmd.get_unchecked(cmd::RESCAN_LOCAL_FILES).clone();
                self.scans_pending += 1;
                data.preferences.local_files.scanning = true;
                let sink = ctx.get_external_handle();
                let widget_id = ctx.widget_id();
                std::thread::spawn(move || {
                    let extra_files = request
                        .username
                        .as_deref()
                        .and_then(|username| spotify_local_file_paths(username).ok())
                        .unwrap_or_default();
                    let summary = LocalLibrary::global().rescan(&request.folders, &extra_files);
                    let _ = sink.submit_command(Self::SCAN_FINISHED, summary.files, widget_id);
                });
                ctx.set_handled();
            }
            Event::Command(cmd) if cmd.is(Self::SCAN_FINISHED) => {
                self.scans_pending = self.scans_pending.saturating_sub(1);
                data.preferences.local_files.files = *cmd.get_unchecked(Self::SCAN_FINISHED);
                data.preferences.local_files.scanning = self.scans_pending > 0;
                // Re-match the local items of the open playlist.
                if let Nav::PlaylistDetail(link) = &data.nav {
                    if data.playlist_detail.tracks.is_resolved() {
                        ctx.submit_command(playlist::LOAD_DETAIL.with((link.clone(), data.clone())));
                    }
                }
                ctx.set_handled();
            }
            _ => child.event(ctx, event, data, env),
        }
    }

    fn lifecycle(
        &mut self,
        child: &mut W,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &AppState,
        env: &Env,
    ) {
        if let LifeCycle::WidgetAdded = event {
            ctx.submit_command(cmd::RESCAN_LOCAL_FILES.with(data.config.local_scan_request()));
        }
        child.lifecycle(ctx, event, data, env);
    }
}
