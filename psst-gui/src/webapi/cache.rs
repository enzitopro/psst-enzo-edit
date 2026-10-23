use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, File},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};

use druid::image;
use druid::ImageBuf;
use lru::LruCache;
use parking_lot::Mutex;
use psst_core::cache::mkdir_if_not_exists;

/// Decoded covers kept in memory: by size, as one 640 px cover takes as much as
/// a hundred 64 px ones.
const IMAGE_CACHE_BYTES: usize = 48 * 1024 * 1024;

pub struct WebApiCache {
    base: Option<PathBuf>,
    /// The covers, and how many bytes they take.
    images: Mutex<(LruCache<Arc<str>, ImageBuf>, usize)>,
}

fn image_bytes(image: &ImageBuf) -> usize {
    image.raw_pixels().len()
}

impl WebApiCache {
    pub fn new(base: Option<PathBuf>) -> Self {
        Self {
            base,
            images: Mutex::new((LruCache::unbounded(), 0)),
        }
    }

    pub fn get_image(&self, uri: &Arc<str>) -> Option<ImageBuf> {
        self.images.lock().0.get(uri).cloned()
    }

    /// Keeps `image`, dropping the least recently shown past the budget.
    pub fn set_image(&self, uri: Arc<str>, image: ImageBuf) {
        let (images, bytes) = &mut *self.images.lock();
        *bytes += image_bytes(&image);
        if let Some(replaced) = images.put(uri, image) {
            *bytes -= image_bytes(&replaced);
        }
        while *bytes > IMAGE_CACHE_BYTES {
            let Some((_, dropped)) = images.pop_lru() else {
                break;
            };
            *bytes -= image_bytes(&dropped);
        }
    }

    pub fn get_image_from_disk(&self, uri: &Arc<str>) -> Option<ImageBuf> {
        let hash = Self::hash_uri(uri);
        self.key("images", &format!("{hash:016x}"))
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(ImageBuf::from_dynamic_image)
    }

    pub fn save_image_to_disk(&self, uri: &Arc<str>, data: &[u8]) {
        let hash = Self::hash_uri(uri);
        if let Some(path) = self.key("images", &format!("{hash:016x}")) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, data);
        }
    }

    fn hash_uri(uri: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        uri.hash(&mut hasher);
        hasher.finish()
    }

    pub fn get(&self, bucket: &str, key: &str) -> Option<File> {
        self.key(bucket, key).and_then(|path| File::open(path).ok())
    }

    pub fn set(&self, bucket: &str, key: &str, value: &[u8]) {
        if let Some(path) = self.bucket(bucket) {
            if let Err(err) = mkdir_if_not_exists(&path) {
                log::error!("failed to create WebAPI cache bucket: {err:?}");
            }
        }
        if let Some(path) = self.key(bucket, key) {
            if let Err(err) = fs::write(path, value) {
                log::error!("failed to save to WebAPI cache: {err:?}");
            }
        }
    }

    fn bucket(&self, bucket: &str) -> Option<PathBuf> {
        self.base.as_ref().map(|path| path.join(bucket))
    }

    fn key(&self, bucket: &str, key: &str) -> Option<PathBuf> {
        self.bucket(bucket).map(|path| path.join(key))
    }
}

#[cfg(test)]
mod tests {
    use druid::piet::ImageFormat;

    use super::*;

    fn image(side: usize) -> ImageBuf {
        let pixels = vec![0; side * side * 4];
        ImageBuf::from_raw(pixels, ImageFormat::RgbaSeparate, side, side)
    }

    #[test]
    fn keeps_covers_within_the_budget() {
        let cache = WebApiCache::new(None);
        // 640 px covers take 1.6 MB each: 30 of them is 48 MB.
        for n in 0..40 {
            cache.set_image(format!("cover {n}").into(), image(640));
        }
        let (images, bytes) = &*cache.images.lock();
        assert!(*bytes <= IMAGE_CACHE_BYTES);
        assert_eq!(*bytes, images.len() * 640 * 640 * 4);
        // The newest stay, the oldest went.
        assert!(images.contains(&Arc::from("cover 39")));
        assert!(!images.contains(&Arc::from("cover 0")));
    }
}
