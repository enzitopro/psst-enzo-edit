//! Index of the user's local music folders, used to play the local files
//! (`spotify:local:...` items) that show up in playlists.
//!
//! Spotify only gives us the metadata it read from the file when it was added
//! to the playlist (title, artist, album and duration), never the path, so we
//! scan the folders configured in the preferences, read the tags of every audio
//! file, and match the playlist items against that.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    thread,
    time::{Duration, UNIX_EPOCH},
};

use druid::im::Vector;
use lofty::{
    config::ParseOptions,
    prelude::{Accessor, AudioFile, TaggedFileExt},
    probe::Probe,
};
use parking_lot::{Mutex, RwLock};
use psst_core::item_id::{ItemId, ItemIdType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::data::{AlbumLink, ArtistLink, Track, TrackId};

pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "oga", "wav"];

const INDEX_FILENAME: &str = "local-files-index.json";
const INDEX_VERSION: u32 = 1;
const DURATION_TOLERANCE_MS: u64 = 2_000;

/// ID given to local playlist items that don't match any file.  The local item
/// registry starts at 1, so this never collides with a playable file.
pub const UNMATCHED_LOCAL_ID: ItemId = ItemId::new(0, ItemIdType::LocalFile);

static GLOBAL: OnceLock<LocalLibrary> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalFile {
    pub path: PathBuf,
    size: u64,
    modified: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    files: Vec<LocalFile>,
}

/// How a playlist item was matched to a file, from most to least reliable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    TitleAndArtist,
    Duration,
    FileName,
}

/// A local item as Spotify describes it.
#[derive(Clone, Debug, Default)]
pub struct LocalItem {
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: Option<u64>,
}

impl LocalItem {
    /// Parses `spotify:local:<artist>:<album>:<title>:<seconds>`, where each
    /// component is URL-encoded with `+` for spaces.
    pub fn from_uri(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("spotify:local:")?;
        let mut parts = rest.split(':').map(|part| {
            let part = part.replace('+', " ");
            urlencoding::decode(&part)
                .map(|s| s.into_owned())
                .unwrap_or(part)
        });
        let artist = parts.next()?;
        let album = parts.next()?;
        let title = parts.next()?;
        let duration_ms = parts
            .next()
            .and_then(|secs| secs.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map(|secs| secs * 1000);
        Some(Self {
            title,
            artists: if artist.is_empty() {
                Vec::new()
            } else {
                vec![artist]
            },
            album,
            duration_ms,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanSummary {
    pub files: usize,
    pub read: usize,
}

pub struct LocalLibrary {
    index: RwLock<Arc<LocalIndex>>,
    cache_path: Option<PathBuf>,
    scanning: Mutex<()>,
}

impl LocalLibrary {
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        let cache_path = cache_dir.map(|dir| dir.join(INDEX_FILENAME));
        let files = cache_path
            .as_deref()
            .and_then(|path| match load_index_file(path) {
                Ok(files) => Some(files),
                Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                Err(err) => {
                    log::warn!("failed to load local files index: {err}");
                    None
                }
            })
            .unwrap_or_default();
        Self {
            index: RwLock::new(Arc::new(LocalIndex::new(files))),
            cache_path,
            scanning: Mutex::new(()),
        }
    }

    pub fn install_as_global(self) {
        GLOBAL
            .set(self)
            .unwrap_or_else(|_| panic!("Cannot install more than once"))
    }

    pub fn global() -> &'static Self {
        GLOBAL.get().unwrap()
    }

    pub fn file_count(&self) -> usize {
        self.index.read().files.len()
    }

    /// Re-scans `folders` (recursively) plus any `extra_files`, re-reading tags
    /// only of files that changed since the last scan, and saves the result.
    pub fn rescan(&self, folders: &[PathBuf], extra_files: &[PathBuf]) -> ScanSummary {
        // Running two scans at once would only waste work.
        let _scanning = self.scanning.lock();

        let mut paths = Vec::new();
        for folder in folders {
            collect_audio_files(folder, &mut paths);
        }
        paths.extend(extra_files.iter().filter(|p| p.is_file()).cloned());
        let mut seen = HashSet::new();
        paths.retain(|path| seen.insert(path.clone()));

        let previous = self.index.read().clone();
        let previous: HashMap<&Path, &LocalFile> = previous
            .files
            .iter()
            .map(|file| (file.path.as_path(), file))
            .collect();

        let mut files = Vec::with_capacity(paths.len());
        let mut to_read = Vec::new();
        for path in paths {
            let Some((size, modified)) = file_stamp(&path) else {
                continue;
            };
            match previous.get(path.as_path()) {
                Some(file) if file.size == size && file.modified == modified => {
                    files.push((*file).clone())
                }
                _ => to_read.push((path, size, modified)),
            }
        }
        let read = to_read.len();
        files.extend(read_files_in_parallel(to_read));
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let summary = ScanSummary {
            files: files.len(),
            read,
        };
        if let Some(path) = &self.cache_path {
            if let Err(err) = save_index_file(path, &files) {
                log::warn!("failed to save local files index: {err}");
            }
        }
        *self.index.write() = Arc::new(LocalIndex::new(files));
        log::info!(
            "indexed {} local files ({} read, {} from cache)",
            summary.files,
            summary.read,
            summary.files - summary.read
        );
        summary
    }

    pub fn find(&self, item: &LocalItem) -> Option<(LocalFile, MatchKind)> {
        let index = self.index.read().clone();
        index
            .find(item)
            .map(|(i, kind)| (index.files[i].clone(), kind))
    }

    /// Builds a track out of a local playlist item, whether it matches a file
    /// or not.  Returns `None` for items that aren't local at all.
    pub fn track_from_playlist_item(&self, json: &Value) -> Option<Arc<Track>> {
        let item: LocalItemJson = match serde_json::from_value(json.clone()) {
            Ok(item) => item,
            Err(err) => {
                log::error!("error parsing local track: {err}");
                return None;
            }
        };
        let is_local = item.is_local
            || item
                .uri
                .as_deref()
                .is_some_and(|uri| uri.starts_with("spotify:local:"));
        if !is_local {
            return None;
        }
        Some(self.track_for_item(
            &item.to_local_item(),
            item.disc_number.unwrap_or(0),
            item.track_number.unwrap_or(0),
        ))
    }

    /// Re-resolves a track that was deserialized as a regular one, but is
    /// flagged as local.
    pub fn track_from_local_track(&self, track: &Track) -> Arc<Track> {
        let item = LocalItem {
            title: track.name.to_string(),
            artists: track.artists.iter().map(|a| a.name.to_string()).collect(),
            album: track
                .album
                .as_ref()
                .map(|album| album.name.to_string())
                .unwrap_or_default(),
            duration_ms: Some(track.duration.as_millis() as u64).filter(|ms| *ms > 0),
        };
        self.track_for_item(&item, track.disc_number, track.track_number)
    }

    pub fn track_for_item(
        &self,
        item: &LocalItem,
        disc_number: usize,
        track_number: usize,
    ) -> Arc<Track> {
        let matched = self.find(item);
        match &matched {
            Some((file, kind)) => log::debug!(
                "local track {:?} matched by {kind:?}: {:?}",
                item.title,
                file.path
            ),
            None => log::info!("local track {:?} not found", item.title),
        }
        Arc::new(local_track(
            item,
            matched.map(|(file, _)| file),
            disc_number,
            track_number,
        ))
    }
}

pub fn local_track(
    item: &LocalItem,
    file: Option<LocalFile>,
    disc_number: usize,
    track_number: usize,
) -> Track {
    let duration_ms = item
        .duration_ms
        .or_else(|| file.as_ref().and_then(|f| f.duration_ms))
        .unwrap_or(0);
    Track {
        id: TrackId(
            file.as_ref()
                .map_or(UNMATCHED_LOCAL_ID, |f| ItemId::from_local(f.path.clone())),
        ),
        name: item.title.as_str().into(),
        album: (!item.album.is_empty()).then(|| AlbumLink {
            id: "".into(),
            name: item.album.as_str().into(),
            images: Vector::new(),
        }),
        artists: item
            .artists
            .iter()
            .map(|name| ArtistLink {
                id: "".into(),
                name: name.as_str().into(),
            })
            .collect(),
        duration: Duration::from_millis(duration_ms),
        disc_number,
        track_number,
        explicit: false,
        is_local: true,
        local_path: file
            .as_ref()
            .map(|f| f.path.to_string_lossy().as_ref().into()),
        is_playable: Some(file.is_some()),
        popularity: None,
        track_pos: 0,
        lyrics: None,
    }
}

// Spotify returns partly bogus data for local tracks (`null` IDs, empty album
// links, ...), so everything here is optional.
#[derive(Deserialize)]
struct LocalItemJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    artists: Option<Vec<NameJson>>,
    #[serde(default)]
    album: Option<NameJson>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    is_local: bool,
    #[serde(default)]
    disc_number: Option<usize>,
    #[serde(default)]
    track_number: Option<usize>,
}

#[derive(Deserialize)]
struct NameJson {
    #[serde(default)]
    name: Option<String>,
}

impl LocalItemJson {
    fn to_local_item(&self) -> LocalItem {
        let from_uri = self
            .uri
            .as_deref()
            .and_then(LocalItem::from_uri)
            .unwrap_or_default();
        let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
        let artists: Vec<String> = self
            .artists
            .iter()
            .flatten()
            .filter_map(|artist| non_empty(&artist.name))
            .collect();
        LocalItem {
            title: non_empty(&self.name).unwrap_or(from_uri.title),
            artists: if artists.is_empty() {
                from_uri.artists
            } else {
                artists
            },
            album: self
                .album
                .as_ref()
                .and_then(|album| non_empty(&album.name))
                .unwrap_or(from_uri.album),
            duration_ms: self
                .duration_ms
                .filter(|ms| *ms > 0)
                .or(from_uri.duration_ms),
        }
    }
}

/// Normalized metadata of an indexed file.
struct FileKeys {
    title: String,
    title_base: String,
    artists: Vec<String>,
    album: String,
    stem: String,
}

pub struct LocalIndex {
    files: Vec<LocalFile>,
    keys: Vec<FileKeys>,
    by_title: HashMap<String, Vec<usize>>,
}

impl LocalIndex {
    pub fn new(files: Vec<LocalFile>) -> Self {
        let keys: Vec<FileKeys> = files
            .iter()
            .map(|file| {
                let stem = file
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let title = file.title.as_deref().unwrap_or("");
                FileKeys {
                    title: normalize(title),
                    title_base: base_title(title),
                    artists: split_artists(file.artist.as_deref().unwrap_or("")),
                    album: normalize(file.album.as_deref().unwrap_or("")),
                    stem: normalize(&stem),
                }
            })
            .collect();
        let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, key) in keys.iter().enumerate() {
            if !key.title_base.is_empty() {
                by_title.entry(key.title_base.clone()).or_default().push(i);
            }
        }
        Self {
            files,
            keys,
            by_title,
        }
    }

    /// Tries, in order: normalized title + artist, duration within ±2 s (for
    /// files that share the title, album or artist), and the file name.
    pub fn find(&self, item: &LocalItem) -> Option<(usize, MatchKind)> {
        let title = normalize(&item.title);
        let title_base = base_title(&item.title);
        let artists: Vec<String> = item.artists.iter().flat_map(|a| split_artists(a)).collect();
        let album = normalize(&item.album);
        let duration_diff = |i: usize| match (item.duration_ms, self.files[i].duration_ms) {
            (Some(a), Some(b)) => a.abs_diff(b),
            _ => u64::MAX,
        };

        // 1. Title and artist.
        if let Some(candidates) = self.by_title.get(&title_base) {
            let best = candidates
                .iter()
                .copied()
                .filter(|&i| artists_match(&artists, &self.keys[i].artists))
                .min_by_key(|&i| {
                    let key = &self.keys[i];
                    (key.title != title, key.album != album, duration_diff(i))
                });
            if let Some(i) = best {
                return Some((i, MatchKind::TitleAndArtist));
            }
        }

        // 2. Duration, but only among files with a similar title or the same
        // album.  The same artist alone isn't enough, any of their songs of
        // about the same length would do.
        if item.duration_ms.is_some() {
            let best = (0..self.files.len())
                .filter(|&i| duration_diff(i) <= DURATION_TOLERANCE_MS)
                .filter_map(|i| {
                    let key = &self.keys[i];
                    let same_title = titles_similar(&title_base, &key.title_base);
                    let same_album = !album.is_empty() && album == key.album;
                    (same_title || same_album).then_some((i, (!same_title, duration_diff(i))))
                })
                .min_by_key(|(_, rank)| *rank);
            if let Some((i, _)) = best {
                return Some((i, MatchKind::Duration));
            }
        }

        // 3. File name, e.g. `Artist - Title.mp3` or `01 Title.flac`.
        if title_base.chars().count() >= 2 {
            let best = (0..self.files.len())
                .filter(|&i| contains_words(&self.keys[i].stem, &title_base))
                .min_by_key(|&i| {
                    let stem = &self.keys[i].stem;
                    let has_artist = artists.iter().any(|a| contains_words(stem, a));
                    (!has_artist, duration_diff(i), stem.len())
                });
            if let Some(i) = best {
                return Some((i, MatchKind::FileName));
            }
        }

        None
    }
}

/// Lowercases, strips accents and punctuation, and collapses whitespace:
/// `"Canción (Remix) — Live!"` becomes `"cancion remix live"`.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(c.to_lowercase());
        } else if c != '\'' && c != '’' {
            // Apostrophes are dropped without splitting the word ("don't").
            pending_space = true;
        }
    }
    out
}

/// The title without version suffixes or featured artists, which Spotify and
/// file tags often spell differently: `"Song (feat. X) - 2011 Remaster"`
/// becomes `"song"`.
pub fn base_title(s: &str) -> String {
    let mut base = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => base.push(c),
            _ => {}
        }
    }
    if let Some(i) = base.find(" - ") {
        base.truncate(i);
    }
    let lower = base.to_lowercase();
    // `to_lowercase` changes byte offsets for some scripts, skip those.
    if lower.len() == base.len() {
        for marker in [" feat.", " feat ", " ft.", " ft ", " featuring "] {
            if let Some(i) = lower.find(marker).filter(|&i| base.is_char_boundary(i)) {
                base.truncate(i);
                break;
            }
        }
    }
    let normalized = normalize(&base);
    if normalized.is_empty() {
        normalize(s)
    } else {
        normalized
    }
}

/// Splits `"A, B & C feat. D"` into normalized individual names.
pub fn split_artists(s: &str) -> Vec<String> {
    let mut s = format!(" {} ", s.to_lowercase());
    for separator in [
        ",", "&", ";", "/", " x ", " feat.", " feat ", " ft.", " ft ", " featuring ", " with ",
        " and ", " vs. ", " vs ",
    ] {
        s = s.replace(separator, "\u{0}");
    }
    s.split('\u{0}')
        .map(normalize)
        .filter(|name| !name.is_empty())
        .collect()
}

pub fn artists_match(query: &[String], file: &[String]) -> bool {
    if query.is_empty() || file.is_empty() {
        // One side has no artist at all; let the title decide.
        return query.is_empty() || file.is_empty();
    }
    query.iter().any(|q| {
        file.iter()
            .any(|f| q == f || (q.len() >= 3 && f.len() >= 3 && (f.contains(q) || q.contains(f))))
    })
}

pub fn titles_similar(a: &str, b: &str) -> bool {
    !a.is_empty()
        && !b.is_empty()
        && (a == b || (a.len() >= 3 && b.len() >= 3 && (contains_words(a, b) || contains_words(b, a))))
}

/// Whether `haystack` contains `needle` as whole words.
fn contains_words(haystack: &str, needle: &str) -> bool {
    !needle.is_empty() && format!(" {haystack} ").contains(&format!(" {needle} "))
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

fn collect_audio_files(root: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!("cannot read local folder {dir:?}: {err}");
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            // Symlinked directories are skipped to avoid loops.
            if file_type.is_dir() {
                stack.push(path);
            } else if is_audio_file(&path) {
                out.push(path);
            }
        }
    }
}

fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    Some((metadata.len(), modified))
}

fn read_files_in_parallel(to_read: Vec<(PathBuf, u64, u64)>) -> Vec<LocalFile> {
    if to_read.is_empty() {
        return Vec::new();
    }
    let workers = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let chunk_size = to_read.len().div_ceil(workers);
    thread::scope(|scope| {
        let handles: Vec<_> = to_read
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(path, size, modified)| read_file(path, *size, *modified))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

pub fn read_file(path: &Path, size: u64, modified: u64) -> LocalFile {
    let mut file = LocalFile {
        path: path.to_path_buf(),
        size,
        modified,
        title: None,
        artist: None,
        album: None,
        duration_ms: None,
    };
    let options = ParseOptions::new().read_cover_art(false);
    let tagged = Probe::open(path)
        .map_err(|err| err.to_string())
        .and_then(|probe| {
            probe
                .options(options)
                .guess_file_type()
                .map_err(|err| err.to_string())
        })
        .and_then(|probe| probe.read().map_err(|err| err.to_string()));
    match tagged {
        Ok(tagged) => {
            let duration = tagged.properties().duration();
            if !duration.is_zero() {
                file.duration_ms = Some(duration.as_millis() as u64);
            }
            if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                let clean = |s: Option<std::borrow::Cow<str>>| {
                    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
                };
                file.title = clean(tag.title());
                file.artist = clean(tag.artist());
                file.album = clean(tag.album());
            }
        }
        Err(err) => log::warn!("cannot read tags of {path:?}: {err}"),
    }
    file
}

fn load_index_file(path: &Path) -> io::Result<Vec<LocalFile>> {
    let file = fs::File::open(path)?;
    let index: IndexFile = serde_json::from_reader(io::BufReader::new(file))?;
    if index.version != INDEX_VERSION {
        return Ok(Vec::new());
    }
    Ok(index.files)
}

fn save_index_file(path: &Path, files: &[LocalFile]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let index = IndexFile {
        version: INDEX_VERSION,
        files: files.to_vec(),
    };
    fs::write(&tmp_path, serde_json::to_vec(&index)?)?;
    fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, title: &str, artist: &str, album: &str, secs: u64) -> LocalFile {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
        LocalFile {
            path: PathBuf::from(path),
            size: 0,
            modified: 0,
            title: some(title),
            artist: some(artist),
            album: some(album),
            duration_ms: (secs > 0).then_some(secs * 1000),
        }
    }

    fn item(title: &str, artist: &str, album: &str, secs: u64) -> LocalItem {
        LocalItem {
            title: title.into(),
            artists: vec![artist.into()],
            album: album.into(),
            duration_ms: (secs > 0).then_some(secs * 1000),
        }
    }

    #[test]
    fn normalizes_text() {
        assert_eq!(normalize("Canción (Remix) — Live!"), "cancion remix live");
        assert_eq!(normalize("  Don't   Stop  "), "dont stop");
        assert_eq!(normalize("Beyoncé"), "beyonce");
        assert_eq!(base_title("Song (feat. X) - 2011 Remaster"), "song");
        assert_eq!(base_title("Song feat. Someone"), "song");
        assert_eq!(base_title("(Intro)"), "intro");
        assert_eq!(
            split_artists("Daft Punk & Pharrell Williams feat. Nile Rodgers"),
            vec!["daft punk", "pharrell williams", "nile rodgers"]
        );
    }

    #[test]
    fn parses_local_uri() {
        let item = LocalItem::from_uri(
            "spotify:local:Los+Prisioneros:Pateando+Piedras:%C2%BFQui%C3%A9n+mat%C3%B3+a+Marilyn%3F:213",
        )
        .unwrap();
        assert_eq!(item.artists, vec!["Los Prisioneros"]);
        assert_eq!(item.album, "Pateando Piedras");
        assert_eq!(item.title, "¿Quién mató a Marilyn?");
        assert_eq!(item.duration_ms, Some(213_000));

        let empty = LocalItem::from_uri("spotify:local:::Untitled:0").unwrap();
        assert!(empty.artists.is_empty());
        assert_eq!(empty.duration_ms, None);
    }

    #[test]
    fn matches_title_and_artist() {
        let index = LocalIndex::new(vec![
            file("a.mp3", "Song", "Other Artist", "", 200),
            file("b.flac", "SONG (Remastered)", "The Artist", "Album", 201),
        ]);
        let (i, kind) = index.find(&item("Song", "the artist", "", 0)).unwrap();
        assert_eq!((i, kind), (1, MatchKind::TitleAndArtist));
    }

    #[test]
    fn falls_back_to_duration() {
        let index = LocalIndex::new(vec![
            file("a.mp3", "Track 1", "Unknown", "My Album", 180),
            file("b.mp3", "Track 2", "Unknown", "My Album", 240),
        ]);
        // Title differs entirely, but album and duration (within 2 s) agree.
        let (i, kind) = index
            .find(&item("Real Name", "Someone", "My Album", 241))
            .unwrap();
        assert_eq!((i, kind), (1, MatchKind::Duration));
        // 3 s off is too far.
        assert!(index.find(&item("Real Name", "Someone", "My Album", 243)).is_none());
    }

    #[test]
    fn falls_back_to_file_name() {
        let index = LocalIndex::new(vec![
            file("C:/Music/01 Intro.mp3", "", "", "", 0),
            file("C:/Music/Some Band - Night Drive.m4a", "", "", "", 0),
            file("C:/Music/Night Driver.m4a", "", "", "", 0),
        ]);
        let (i, kind) = index
            .find(&item("Night Drive", "Some Band", "", 0))
            .unwrap();
        assert_eq!((i, kind), (1, MatchKind::FileName));
        let (i, _) = index.find(&item("Intro", "", "", 0)).unwrap();
        assert_eq!(i, 0);
    }

    #[test]
    fn same_artist_and_length_is_not_a_match() {
        let index = LocalIndex::new(vec![file("a.mp3", "Other Song", "Yeat", "Album A", 180)]);
        assert!(index
            .find(&item("Unknown Title", "Yeat", "Album B", 181))
            .is_none());
    }

    #[test]
    fn reports_missing_files() {
        let index = LocalIndex::new(vec![file("a.mp3", "Song", "Artist", "Album", 200)]);
        assert!(index
            .find(&item("Completely Different", "Nobody", "Nothing", 30))
            .is_none());
    }

    #[test]
    fn builds_tracks_from_playlist_json() {
        let library = LocalLibrary {
            index: RwLock::new(Arc::new(LocalIndex::new(vec![file(
                "C:/Music/song.mp3",
                "Song",
                "Artist",
                "Album",
                200,
            )]))),
            cache_path: None,
            scanning: Mutex::new(()),
        };
        let found = library
            .track_from_playlist_item(&serde_json::json!({
                "id": null,
                "name": "Song",
                "uri": "spotify:local:Artist:Album:Song:200",
                "artists": [{ "id": null, "name": "Artist" }],
                "album": { "id": null, "name": "Album", "images": [] },
                "duration_ms": 200000,
                "is_local": true,
            }))
            .unwrap();
        assert!(found.is_local);
        assert_eq!(found.is_playable, Some(true));
        assert_eq!(found.id.0.id_type, ItemIdType::LocalFile);
        assert_ne!(found.id.0, UNMATCHED_LOCAL_ID);

        let missing = library
            .track_from_playlist_item(&serde_json::json!({
                "uri": "spotify:local:Nobody:Nothing:Missing+Song:100",
                "is_local": true,
            }))
            .unwrap();
        assert_eq!(missing.is_playable, Some(false));
        assert_eq!(&*missing.name, "Missing Song");
        assert_eq!(missing.id.0, UNMATCHED_LOCAL_ID);

        // Bogus non-local data is still skipped.
        assert!(library
            .track_from_playlist_item(&serde_json::json!({ "name": "x", "is_local": false }))
            .is_none());
    }
}
