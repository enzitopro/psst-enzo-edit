//! Lyrics from LRCLIB (https://lrclib.net), a free and open lyrics database
//! with synced (LRC) lyrics.  Used for local files, and for Spotify tracks that
//! Spotify has no lyrics for.

use druid::im::Vector;
use url::Url;

use super::{
    local_library::{artists_match, base_title, normalize, split_artists, titles_similar},
    WebApi,
};
use crate::{
    data::{Lyrics, LyricsCandidate, Track, TrackLines},
    error::Error,
};

const API: &str = "https://lrclib.net/api";
/// Cached lyrics per track: the automatic match, or the one picked by hand.
const CACHE_BUCKET: &str = "lrclib";

impl WebApi {
    /// Lyrics for `track`, from (in order): a previous LRCLIB pick, Spotify,
    /// and an LRCLIB lookup.
    pub fn lyrics_for_track(&self, track: &Track) -> Result<Lyrics, Error> {
        let key = cache_key(track);
        if let Some(candidate) = self.cache_get_json::<LyricsCandidate>(CACHE_BUCKET, &key) {
            return Ok(to_lyrics(&candidate));
        }
        // Only remember the LRCLIB match if Spotify really has no lyrics, not
        // when it failed for a moment.
        let mut remember = true;
        if !track.is_local {
            match self.get_lyrics(track.id.0.to_base62()) {
                Ok(lines) if !lines.is_empty() => return Ok(Lyrics::new(lines, "Spotify")),
                Ok(_) => {}
                Err(err) => {
                    log::info!("no Spotify lyrics for {:?}: {err}", track.name);
                    remember = err.to_string().contains("404");
                }
            }
        }
        let candidate = self
            .find_lrclib_lyrics(track)?
            .ok_or_else(|| Error::WebApiError("No lyrics found for this track".to_string()))?;
        if remember {
            self.cache_set_json(CACHE_BUCKET, &key, &candidate);
        }
        Ok(to_lyrics(&candidate))
    }

    /// Free-text search, for picking lyrics by hand.
    pub fn search_lrclib(&self, query: &str) -> Result<Vector<LyricsCandidate>, Error> {
        let url = url_with(&format!("{API}/search"), &[("q", query)])?;
        let results: Vec<LyricsCandidate> = self.get_public_json(&url)?.unwrap_or_default();
        Ok(results.into_iter().filter(LyricsCandidate::has_lyrics).collect())
    }

    /// Remembers the lyrics picked by hand, so they're used from now on.
    pub fn remember_lyrics_choice(&self, track: &Track, candidate: &LyricsCandidate) {
        self.cache_set_json(CACHE_BUCKET, &cache_key(track), candidate);
    }

    fn find_lrclib_lyrics(&self, track: &Track) -> Result<Option<LyricsCandidate>, Error> {
        let title = track.name.to_string();
        let artist = track.artist_name().to_string();
        let album = track.album.as_ref().map(|a| a.name.to_string()).unwrap_or_default();
        let duration = track.duration.as_secs();

        // An exact lookup needs all four fields.
        if !album.is_empty() && duration > 0 {
            let url = url_with(
                &format!("{API}/get"),
                &[
                    ("track_name", &title),
                    ("artist_name", &artist),
                    ("album_name", &album),
                    ("duration", &duration.to_string()),
                ],
            )?;
            if let Some(candidate) = self.get_public_json::<LyricsCandidate>(&url)? {
                if candidate.has_lyrics() {
                    return Ok(Some(candidate));
                }
            }
        }

        let url = url_with(
            &format!("{API}/search"),
            &[("track_name", &title), ("artist_name", &artist)],
        )?;
        let mut results: Vec<LyricsCandidate> = self.get_public_json(&url)?.unwrap_or_default();
        if results.is_empty() {
            // Spelling differences in the artist are common, try once more
            // with a loose query.
            let url = url_with(&format!("{API}/search"), &[("q", &format!("{artist} {title}"))])?;
            results = self.get_public_json(&url)?.unwrap_or_default();
        }
        Ok(best_match(&results, track).cloned())
    }
}

fn url_with(base: &str, params: &[(&str, &str)]) -> Result<String, Error> {
    Url::parse_with_params(base, params)
        .map(String::from)
        .map_err(|err| Error::WebApiError(err.to_string()))
}

/// Key of the lyrics cache, from the track metadata (local files have no
/// stable ID).
fn cache_key(track: &Track) -> String {
    let text = format!("{}|{}", normalize(&track.artist_names()), normalize(&track.name));
    // FNV-1a, stable across builds unlike the std hasher.
    let hash = text.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

/// The search result most likely to be `track`: same title, then same artist,
/// duration within 2 seconds, and synced lyrics preferred.
pub fn best_match<'a>(results: &'a [LyricsCandidate], track: &Track) -> Option<&'a LyricsCandidate> {
    let title = base_title(&track.name);
    let artists = split_artists(&track.artist_names());
    let duration = track.duration.as_secs_f64();
    results
        .iter()
        .filter(|c| c.has_lyrics())
        .filter(|c| titles_similar(&title, &base_title(&c.track_name)))
        .map(|c| {
            let diff = if duration > 0.0 && c.duration > 0.0 {
                (c.duration - duration).abs()
            } else {
                0.0
            };
            (c, diff)
        })
        // Way off in length means another version (live, remix, ...).
        .filter(|(_, diff)| *diff <= 10.0)
        .min_by_key(|(c, diff)| {
            (
                !artists_match(&artists, &split_artists(&c.artist_name)),
                *diff > 2.0,
                !c.is_synced(),
                (*diff * 1000.0) as u64,
            )
        })
        .map(|(c, _)| c)
}

pub fn to_lyrics(candidate: &LyricsCandidate) -> Lyrics {
    if let Some(synced) = candidate.synced_lyrics.as_deref().filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(synced);
        if !lines.is_empty() {
            return Lyrics::new(lines, "LRCLIB");
        }
    }
    if let Some(plain) = candidate.plain_lyrics.as_deref().filter(|s| !s.trim().is_empty()) {
        return Lyrics::new(plain.lines().map(|line| line_at(0, line)), "LRCLIB");
    }
    Lyrics::new([line_at(0, "♪ Instrumental ♪")], "LRCLIB")
}

fn line_at(start_ms: u64, words: &str) -> TrackLines {
    TrackLines {
        start_time_ms: start_ms.to_string(),
        words: if words.trim().is_empty() {
            " ".to_string()
        } else {
            words.trim().to_string()
        },
        end_time_ms: "0".to_string(),
    }
}

/// Parses LRC: `[mm:ss.xx]text` lines, several timestamps per line, an
/// `[offset:±ms]` tag, and enhanced `<mm:ss.xx>` word timestamps (dropped).
pub fn parse_lrc(lrc: &str) -> Vec<TrackLines> {
    let mut offset_ms: i64 = 0;
    let mut timed: Vec<(u64, String)> = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(end) = inner.find(']') else {
                break;
            };
            let tag = &inner[..end];
            rest = &inner[end + 1..];
            if let Some(ms) = parse_timestamp(tag) {
                stamps.push(ms);
            } else if let Some(value) = tag.strip_prefix("offset:") {
                offset_ms = value.trim().parse().unwrap_or(0);
            }
            // Other tags (`[ar:...]`, `[ti:...]`) are metadata.
        }
        let text = strip_word_timestamps(rest);
        let text = if text.is_empty() { "♪".to_string() } else { text };
        for ms in stamps {
            timed.push((ms, text.clone()));
        }
    }
    timed.sort_by_key(|(ms, _)| *ms);
    timed
        .into_iter()
        .map(|(ms, text)| {
            // A positive offset shows the lyrics earlier.
            let ms = (ms as i64 - offset_ms).max(0) as u64;
            line_at(ms, &text)
        })
        .collect()
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `mm:ss:xx` to milliseconds.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let (minutes, rest) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    let seconds: u64 = seconds.trim().parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let fraction_ms = if fraction.is_empty() {
        0
    } else {
        let digits: String = fraction.chars().take(3).collect();
        let value: u64 = digits.parse().ok()?;
        value * 10u64.pow(3 - digits.len() as u32)
    };
    Some(minutes * 60_000 + seconds * 1000 + fraction_ms)
}

fn strip_word_timestamps(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        let tag = &rest[start + 1..start + len];
        if parse_timestamp(tag).is_none() {
            // Not a timestamp, keep it.
            out.push_str(&rest[start..start + len + 1]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::*;
    use crate::data::ArtistLink;

    fn candidate(id: i64, title: &str, artist: &str, secs: f64, synced: bool) -> LyricsCandidate {
        LyricsCandidate {
            id,
            track_name: title.into(),
            artist_name: artist.into(),
            album_name: "".into(),
            duration: secs,
            instrumental: false,
            plain_lyrics: Some("la la".into()),
            synced_lyrics: synced.then(|| Arc::from("[00:01.00]la la")),
        }
    }

    fn track(title: &str, artist: &str, secs: u64) -> Track {
        let mut track = crate::webapi::local_library::local_track(
            &Default::default(),
            None,
            0,
            0,
        );
        track.name = title.into();
        track.artists = [ArtistLink {
            id: "".into(),
            name: artist.into(),
        }]
        .into_iter()
        .collect();
        track.duration = Duration::from_secs(secs);
        track
    }

    #[test]
    fn parses_lrc() {
        let lines = parse_lrc(
            "[ar:Someone]\n[offset:+500]\n[00:12.50]First\n[00:05.00][01:00.00]Chorus <00:05.20>with <00:05.80>words\n[00:20.123]\n",
        );
        let starts: Vec<u64> = lines.iter().map(TrackLines::start_ms).collect();
        let words: Vec<&str> = lines.iter().map(|l| l.words.as_str()).collect();
        assert_eq!(starts, vec![4_500, 12_000, 19_623, 59_500]);
        assert_eq!(words, vec!["Chorus with words", "First", "♪", "Chorus with words"]);

        let lyrics = Lyrics::new(lines, "LRCLIB");
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines[0].end_ms(), 12_000);
        assert_eq!(lyrics.lines[3].end_ms(), u64::MAX);
    }

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("01:02.3"), Some(62_300));
        assert_eq!(parse_timestamp("01:02.34"), Some(62_340));
        assert_eq!(parse_timestamp("01:02.345"), Some(62_345));
        assert_eq!(parse_timestamp("01:02:34"), Some(62_340));
        assert_eq!(parse_timestamp("1:02"), Some(62_000));
        assert_eq!(parse_timestamp("ar:Someone"), None);
        assert_eq!(parse_timestamp("00:75.00"), None);
    }

    #[test]
    fn plain_lyrics_are_not_synced() {
        let mut c = candidate(1, "Song", "Artist", 100.0, false);
        c.plain_lyrics = Some("One\n\nTwo".into());
        let lyrics = to_lyrics(&c);
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[1].words, " ");
    }

    #[test]
    fn picks_the_best_candidate() {
        let results = vec![
            candidate(1, "Marching (Live)", "Yeat", 240.0, true),
            candidate(2, "Marching", "Someone Else", 180.0, true),
            candidate(3, "marching", "Yeat", 185.0, true),
            candidate(4, "Marching", "Yeat", 181.0, false),
            candidate(5, "Marching", "Yeat", 180.5, true),
        ];
        let best = best_match(&results, &track("Marching", "Yeat", 180)).unwrap();
        assert_eq!(best.id, 5);
        // Nothing with the same title.
        assert!(best_match(&results, &track("Other Song", "Yeat", 180)).is_none());
    }

    #[test]
    fn cache_key_ignores_case_and_accents() {
        assert_eq!(
            cache_key(&track("Not Worriëd", "Yeat", 1)),
            cache_key(&track("not worried", "YEAT", 99))
        );
        assert_ne!(
            cache_key(&track("Song", "A", 1)),
            cache_key(&track("Song", "B", 1))
        );
    }
}
