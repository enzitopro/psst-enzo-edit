mod cache;
mod client;
#[cfg(test)]
mod diagnostics;
mod local;
pub mod local_library;
mod lrclib;
mod pathfinder;

pub use client::WebApi;
pub use local::spotify_local_file_paths;
pub use local_library::LocalLibrary;
pub use lrclib::to_lyrics as lyrics_from_candidate;
