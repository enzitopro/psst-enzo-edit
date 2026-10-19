#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::new_without_default, clippy::type_complexity)]

mod cmd;
mod controller;
mod data;
mod delegate;
mod discord;
mod error;
mod ui;
mod webapi;
mod widget;

use std::{
    fs::{self, File},
    io::{self, Write},
};

use druid::AppLauncher;
use env_logger::{Builder, Env};
use webapi::{LocalLibrary, WebApi};

use psst_core::cache::Cache;

use crate::{
    data::{AppState, Config},
    delegate::Delegate,
};

const ENV_LOG: &str = "PSST_LOG";
const ENV_LOG_STYLE: &str = "PSST_LOG_STYLE";

/// `psst.log` next to the config, started fresh on every launch.  The log of
/// the previous launch is kept as `psst.old.log`.
fn open_log_file() -> Option<File> {
    let dir = Config::config_dir()?;
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("psst.log");
    let _ = fs::rename(&path, dir.join("psst.old.log"));
    File::create(path).ok()
}

/// Writes the log both to stderr and to the log file.
struct TeeWriter(File);

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.0.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.0.flush()
    }
}

fn main() {
    // Setup logging from the env variables, with defaults.
    let mut logger = Builder::from_env(
        Env::new()
            .filter_or(ENV_LOG, "info")
            .write_style(ENV_LOG_STYLE),
    );
    // Lofty warns about every MP3 without a duration header.
    logger.filter_module("lofty", log::LevelFilter::Error);
    // Release builds have no console, keep the log in a file as well.
    if let Some(file) = open_log_file() {
        logger.target(env_logger::Target::Pipe(Box::new(TeeWriter(file))));
    }
    logger.init();

    // Load configuration
    let config = Config::load().unwrap_or_default();

    let paginated_limit = config.paginated_limit;
    let mut state = AppState::default_with_config(config.clone());

    if let Some(cache_dir) = Config::cache_dir() {
        match Cache::new(cache_dir) {
            Ok(cache) => {
                state.preferences.cache = Some(cache);
            }
            Err(err) => {
                log::error!("Failed to create cache: {err}");
            }
        }
    }

    WebApi::new(
        Config::proxy().as_deref(),
        Config::cache_dir(),
        paginated_limit,
    )
    .install_as_global();

    // Load the local files index saved by the last scan.  The main window
    // re-scans the configured folders in the background once it opens.
    LocalLibrary::new(Config::cache_dir()).install_as_global();
    state.preferences.local_files.files = LocalLibrary::global().file_count();

    // Share the core session so the WebApi can authenticate `api-partner`
    // (pathfinder GraphQL) calls with first-party tokens.
    WebApi::global().set_session(state.session.clone());

    let delegate;
    let launcher;
    if state.config.has_credentials() {
        // Credentials are configured, open the main window.

        // Check if we have a valid Web API token. If not, try to refresh.
        // If refresh fails, the user can re-authenticate from preferences.
        match state.config.get_or_refresh_webapi_token() {
            Ok(token) => {
                log::info!("Web API token is valid");
                // Seed the global WebApi with the valid token
                WebApi::global().set_webapi_credentials(
                    state.config.webapi_client_id_value().map(String::from),
                    Some(token),
                );
            }
            Err(e) => {
                log::warn!(
                    "No valid Web API token: {e}. \
                     Web API features may be unavailable until re-authentication."
                );
                // Still provide the client ID so refresh can be attempted later
                WebApi::global().set_webapi_credentials(
                    state.config.webapi_client_id_value().map(String::from),
                    None,
                );
            }
        }

        let window = ui::main_window(&state.config);
        delegate = Delegate::with_main(window.id);
        launcher = AppLauncher::with_window(window).configure_env(ui::theme::setup);
    } else {
        // No configured credentials, open the account setup.
        let window = ui::account_setup_window();
        delegate = Delegate::with_preferences(window.id);
        launcher = AppLauncher::with_window(window).configure_env(ui::theme::setup);
    };

    launcher
        .delegate(delegate)
        .launch(state)
        .expect("Application launch");
}
