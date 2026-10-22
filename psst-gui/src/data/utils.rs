use std::{
    convert::TryFrom,
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use druid::{im::Vector, Data, Lens};
use sanitize_html::rules::predefined::DEFAULT;
use sanitize_html::sanitize_str;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::{Date, Month};

#[derive(Clone, Data, Lens)]
pub struct Cached<T: Data> {
    pub data: T,
    #[data(ignore)]
    pub cached_at: Option<SystemTime>,
}

impl<T: Data> Cached<T> {
    pub fn new(data: T, at: SystemTime) -> Self {
        Self {
            data,
            cached_at: Some(at),
        }
    }

    pub fn fresh(data: T) -> Self {
        Self {
            data,
            cached_at: None,
        }
    }

    pub fn map<U: Data>(self, f: impl Fn(T) -> U) -> Cached<U> {
        Cached {
            data: f(self.data),
            cached_at: self.cached_at,
        }
    }
}

#[derive(Deserialize)]
pub struct Page<T: Clone> {
    // Spotify sometimes returns `null` entries (e.g. region-blocked or deleted
    // items), which would otherwise fail the whole page.
    #[serde(
        deserialize_with = "deserialize_skip_nulls",
        bound(deserialize = "T: Deserialize<'de>")
    )]
    pub items: Vector<T>,
    pub limit: usize,
    pub offset: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Data, Deserialize, Serialize)]
pub struct Image {
    pub url: Arc<str>,
    pub width: Option<usize>,
    pub height: Option<usize>,
}

impl Image {
    pub fn fits(&self, width: f64, height: f64) -> bool {
        if let (Some(w), Some(h)) = (self.width, self.height) {
            (w as f64) < width && (h as f64) < height
        } else {
            true // Unknown dimensions, treat as fitting.
        }
    }

    /// The smallest image covering `width` × `height`, else the largest one.
    /// The order doesn't matter: pathfinder lists 300, 64, 640, and taking the
    /// first that covers decoded 640 px covers for 48 px rows, ~1.6 MB each.
    pub fn at_least_of_size(images: &Vector<Self>, width: f64, height: f64) -> Option<&Self> {
        let area = |img: &&Self| img.width.unwrap_or(0) * img.height.unwrap_or(0);
        images
            .iter()
            .filter(|img| !img.fits(width, height))
            .min_by_key(area)
            .or_else(|| images.iter().max_by_key(area))
    }
}

pub fn default_str() -> Arc<str> {
    "".into()
}

#[allow(dead_code)]
pub fn deserialize_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let secs = f64::deserialize(deserializer)?;
    let duration = Duration::from_secs_f64(secs);
    Ok(duration)
}

pub fn serialize_millis<S: Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(duration.as_millis() as u64)
}

pub fn deserialize_millis<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    // Sometimes spotify will provide a negative number for a duration
    let millis = u64::deserialize(deserializer).unwrap_or(0);
    let duration = Duration::from_millis(millis);
    Ok(duration)
}

pub fn deserialize_date<'de, D>(deserializer: D) -> Result<Date, D::Error>
where
    D: Deserializer<'de>,
{
    let date = String::deserialize(deserializer)?;
    let mut parts = date.splitn(3, '-');
    let year = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let month: u8 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
    let month = Month::try_from(month).unwrap_or(Month::January);
    let day = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);

    Date::from_calendar_date(year, month, day)
        .map_err(|_err| serde::de::Error::custom("Invalid date"))
}

pub fn deserialize_date_option<'de, D>(deserializer: D) -> Result<Option<Date>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Wrapper(#[serde(deserialize_with = "deserialize_date")] Date);

    Ok(Option::deserialize(deserializer)?.map(|Wrapper(val)| val))
}

pub fn deserialize_first_page<'de, D, T>(deserializer: D) -> Result<Vector<T>, D::Error>
where
    T: Clone,
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    let page = Page::<T>::deserialize(deserializer)?;
    Ok(page.items)
}

pub fn deserialize_skip_nulls<'de, D, T>(deserializer: D) -> Result<Vector<T>, D::Error>
where
    T: Clone,
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    let items = Option::<Vec<Option<T>>>::deserialize(deserializer)?;
    Ok(items.unwrap_or_default().into_iter().flatten().collect())
}

pub fn deserialize_null_arc_str<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_else(default_str))
}

pub fn sanitize_html_string(text: &str) -> Arc<str> {
    let sanitized = sanitize_str(&DEFAULT, text).unwrap_or_default();
    Arc::from(sanitized.replace("&amp;", "&"))
}

/// Writes `path` through a temporary file swapped in at the end, so a crash or a
/// power cut while writing never leaves it half written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    let mut file = File::create(&tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod image_tests {
    use super::*;

    fn image(size: Option<usize>) -> Image {
        Image {
            url: format!("{size:?}").into(),
            width: size,
            height: size,
        }
    }

    #[test]
    fn picks_the_smallest_cover_that_is_big_enough() {
        let images: Vector<_> = [Some(300), Some(64), Some(640)]
            .map(image)
            .into_iter()
            .collect();
        let picked = |size| Image::at_least_of_size(&images, size, size).unwrap().width;
        assert_eq!(picked(48.0), Some(64));
        assert_eq!(picked(128.0), Some(300));
        assert_eq!(picked(400.0), Some(640));
        assert_eq!(picked(1000.0), Some(640));
        // A playlist mosaic has one image of unknown size.
        let mosaic: Vector<_> = [image(None)].into_iter().collect();
        assert!(Image::at_least_of_size(&mosaic, 48.0, 48.0).is_some());
    }
}
