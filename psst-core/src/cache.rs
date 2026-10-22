use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::{
    audio::decrypt::AudioKey,
    error::Error,
    item_id::{FileId, ItemId, ItemIdType},
};

use librespot_protocol::metadata::{Episode, Track};
use protobuf::Message;
use tempfile::NamedTempFile;

pub type CacheHandle = Arc<Cache>;

#[derive(Debug)]
pub struct Cache {
    base: PathBuf,
}

fn create_cache_dirs(base: &Path) -> io::Result<()> {
    mkdir_if_not_exists(base)?;
    mkdir_if_not_exists(&base.join("track"))?;
    mkdir_if_not_exists(&base.join("episode"))?;
    mkdir_if_not_exists(&base.join("audio"))?;
    mkdir_if_not_exists(&base.join("key"))?;
    mkdir_if_not_exists(&base.join("downloads"))?;
    Ok(())
}

/// What Clear Cache leaves: the downloads, and what they need to play and to
/// show offline (keys, track info, the saved copies of the library).
const KEPT_BY_CLEAR: &[&str] = &[
    "downloads",
    "key",
    "track",
    "episode",
    "offline",
    "offline-playlist",
    "offline-playlists.json",
];

/// The tracks kept from listening take at most this much, the oldest played
/// go first.
pub const AUDIO_CACHE_LIMIT: u64 = 5_000_000_000;

impl Cache {
    pub fn new(base: PathBuf) -> Result<CacheHandle, Error> {
        log::info!("using cache: {base:?}");

        // Create the cache structure.
        create_cache_dirs(&base)?;

        let cache = Self { base };
        Ok(Arc::new(cache))
    }

    /// Everything but the downloads.
    pub fn clear(&self) -> io::Result<()> {
        log::info!("clearing cache: {:?}", self.base);

        for entry in fs::read_dir(&self.base)? {
            let entry = entry?;
            if KEPT_BY_CLEAR.iter().any(|kept| entry.file_name() == *kept) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }

        // Re-create the essential directory structure.
        create_cache_dirs(&self.base)
    }

    pub fn clear_downloads(&self) -> io::Result<()> {
        log::info!("deleting downloads");
        fs::remove_dir_all(self.base.join("downloads"))?;
        let _ = fs::remove_file(self.base.join("offline-playlists.json"));
        create_cache_dirs(&self.base)
    }
}

// Cache of `Track` protobuf structures.
impl Cache {
    pub fn get_track(&self, item_id: ItemId) -> Option<Track> {
        let buf = fs::read(self.track_path(item_id)).ok()?;
        Track::parse_from_bytes(&buf).ok()
    }

    pub fn save_track(&self, item_id: ItemId, track: &Track) -> Result<(), Error> {
        log::debug!("saving track to cache: {item_id:?}");
        fs::write(self.track_path(item_id), track.write_to_bytes()?)?;
        Ok(())
    }

    /// All tracks with cached metadata.
    pub fn track_ids(&self) -> Vec<ItemId> {
        let Ok(entries) = fs::read_dir(self.base.join("track")) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                ItemId::from_base62(name.to_str()?, ItemIdType::Track)
            })
            .collect()
    }

    fn track_path(&self, item_id: ItemId) -> PathBuf {
        self.base.join("track").join(item_id.to_base62())
    }
}

// Cache of `Episode` protobuf structures.
impl Cache {
    pub fn get_episode(&self, item_id: ItemId) -> Option<Episode> {
        let buf = fs::read(self.episode_path(item_id)).ok()?;
        Episode::parse_from_bytes(&buf).ok()
    }

    pub fn save_episode(&self, item_id: ItemId, episode: &Episode) -> Result<(), Error> {
        log::debug!("saving episode to cache: {item_id:?}");
        fs::write(self.episode_path(item_id), episode.write_to_bytes()?)?;
        Ok(())
    }

    fn episode_path(&self, item_id: ItemId) -> PathBuf {
        self.base.join("episode").join(item_id.to_base62())
    }
}

// Cache of `AudioKey`s.
impl Cache {
    pub fn get_audio_key(&self, item_id: ItemId, file_id: FileId) -> Option<AudioKey> {
        let buf = fs::read(self.audio_key_path(item_id, file_id)).ok()?;
        AudioKey::from_raw(&buf)
    }

    pub fn save_audio_key(
        &self,
        item_id: ItemId,
        file_id: FileId,
        key: &AudioKey,
    ) -> Result<(), Error> {
        log::debug!("saving audio key to cache: {item_id:?}:{file_id:?}");
        fs::write(self.audio_key_path(item_id, file_id), key.0)?;
        Ok(())
    }

    fn audio_key_path(&self, item_id: ItemId, file_id: FileId) -> PathBuf {
        let mut key_id = String::new();
        key_id += &item_id.to_base62()[..16];
        key_id += &file_id.to_base16()[..16];
        self.base.join("key").join(key_id)
    }
}

// Encrypted audio files: the downloads, and the cache of what played.
impl Cache {
    pub fn audio_file_path(&self, file_id: FileId) -> PathBuf {
        self.base.join("audio").join(file_id.to_base16())
    }

    /// Where a download is kept, apart from the cache.
    pub fn download_file_path(&self, file_id: FileId) -> PathBuf {
        self.base.join("downloads").join(file_id.to_base16())
    }

    /// Moves a file played to the end into the downloads, instead of fetching
    /// it again.  Whether the download is there now.
    pub fn adopt_download(&self, file_id: FileId) -> bool {
        let download = self.download_file_path(file_id);
        download.exists() || fs::rename(self.audio_file_path(file_id), download).is_ok()
    }

    /// Marks a cached file as just played, so the limit removes it last.
    pub fn touch_audio_file(&self, file_id: FileId) {
        let file = fs::File::options()
            .write(true)
            .open(self.audio_file_path(file_id));
        if let Err(err) = file.and_then(|file| file.set_modified(SystemTime::now())) {
            log::debug!("couldn't mark {file_id:?} as played: {err}");
        }
    }

    /// Removes the least recently played files until the cache is under `limit`.
    pub fn trim_audio_files(&self, limit: u64) -> io::Result<()> {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = fs::read_dir(self.base.join("audio"))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                Some((metadata.modified().ok()?, metadata.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        files.sort();
        for (_, size, path) in files {
            if total <= limit {
                break;
            }
            fs::remove_file(&path)?;
            total -= size;
        }
        Ok(())
    }

    pub fn save_audio_file(&self, file_id: FileId, from_path: PathBuf) -> Result<(), Error> {
        log::debug!("saving audio file to cache: {file_id:?}");
        // Copy next to it and rename, so closing Psst mid-copy never leaves a
        // truncated file that would be played (and counted as downloaded).
        let mut file = NamedTempFile::new_in(self.base.join("audio"))?;
        io::copy(&mut fs::File::open(from_path)?, &mut file)?;
        file.persist(self.audio_file_path(file_id))
            .map_err(|err| err.error)?;
        if let Err(err) = self.trim_audio_files(AUDIO_CACHE_LIMIT) {
            log::warn!("couldn't trim the audio cache: {err}");
        }
        Ok(())
    }
}

// Cache of user country code.
impl Cache {
    pub fn get_country_code(&self) -> Option<String> {
        fs::read_to_string(self.country_code_path()).ok()
    }

    pub fn save_country_code(&self, country_code: &str) -> Result<(), Error> {
        fs::write(self.country_code_path(), country_code)?;
        Ok(())
    }

    fn country_code_path(&self) -> PathBuf {
        self.base.join("country_code")
    }
}

pub fn mkdir_if_not_exists(path: &Path) -> io::Result<()> {
    fs::create_dir(path).or_else(|err| {
        if err.kind() == io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(err)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn file_id(n: u8) -> FileId {
        FileId([n; 20])
    }

    #[test]
    fn downloads_survive_clearing_and_the_cache_keeps_its_limit() {
        let base = std::env::temp_dir().join(format!("psst-cache-split-{}", std::process::id()));
        let cache = Cache::new(base.clone()).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        for n in 1..=3 {
            fs::write(cache.audio_file_path(file_id(n)), [0; 10]).unwrap();
            let file = fs::File::options()
                .write(true)
                .open(cache.audio_file_path(file_id(n)));
            file.unwrap()
                .set_modified(old + Duration::from_secs(n as u64))
                .unwrap();
        }
        // Played just now, so kept over the older ones.
        cache.touch_audio_file(file_id(1));
        cache.trim_audio_files(20).unwrap();
        assert!(cache.audio_file_path(file_id(1)).exists());
        assert!(!cache.audio_file_path(file_id(2)).exists());
        assert!(cache.audio_file_path(file_id(3)).exists());

        // A played file becomes a download without fetching it again.
        assert!(cache.adopt_download(file_id(3)));
        assert!(cache.download_file_path(file_id(3)).exists());
        assert!(!cache.audio_file_path(file_id(3)).exists());
        assert!(!cache.adopt_download(file_id(9)));

        fs::write(base.join("offline-playlists.json"), "{}").unwrap();
        cache.clear().unwrap();
        assert!(!cache.audio_file_path(file_id(1)).exists());
        assert!(cache.download_file_path(file_id(3)).exists());
        assert!(base.join("offline-playlists.json").exists());

        cache.clear_downloads().unwrap();
        assert!(!cache.download_file_path(file_id(3)).exists());
        assert!(!base.join("offline-playlists.json").exists());
        assert!(base.join("downloads").is_dir());
        fs::remove_dir_all(&base).unwrap();
    }
}
