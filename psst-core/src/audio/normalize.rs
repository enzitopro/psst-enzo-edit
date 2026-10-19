use std::{
    io,
    io::{Read, Seek, SeekFrom},
};

use byteorder::{ReadBytesExt, LE};
use symphonia::core::meta::{MetadataRevision, StandardTagKey};

/// ReplayGain values are relative to a -18 LUFS reference, while Spotify
/// normalizes to -14 LUFS.  Shift local files up so both play equally loud.
const REPLAY_GAIN_TO_SPOTIFY_DB: f32 = 4.0;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NormalizationLevel {
    None,
    Track,
    Album,
}

#[derive(Clone, Copy)]
pub struct NormalizationData {
    track_gain_db: f32,
    track_peak: f32,
    album_gain_db: f32,
    album_peak: f32,
}

impl NormalizationData {
    pub fn parse(mut file: impl Read + Seek) -> io::Result<Self> {
        const NORMALIZATION_OFFSET: u64 = 144;

        file.seek(SeekFrom::Start(NORMALIZATION_OFFSET))?;

        let track_gain_db = file.read_f32::<LE>()?;
        let track_peak = file.read_f32::<LE>()?;
        let album_gain_db = file.read_f32::<LE>()?;
        let album_peak = file.read_f32::<LE>()?;

        Ok(Self {
            track_gain_db,
            track_peak,
            album_gain_db,
            album_peak,
        })
    }

    pub fn from_replay_gain(
        track_gain_db: f32,
        track_peak: f32,
        album_gain_db: f32,
        album_peak: f32,
    ) -> Self {
        Self {
            track_gain_db: track_gain_db + REPLAY_GAIN_TO_SPOTIFY_DB,
            track_peak,
            album_gain_db: album_gain_db + REPLAY_GAIN_TO_SPOTIFY_DB,
            album_peak,
        }
    }

    pub fn factor_for_level(&self, level: NormalizationLevel, pregain: f32) -> f32 {
        match level {
            NormalizationLevel::None => 1.0,
            NormalizationLevel::Track => Self::factor(pregain, self.track_gain_db, self.track_peak),
            NormalizationLevel::Album => Self::factor(pregain, self.album_gain_db, self.album_peak),
        }
    }

    fn factor(pregain: f32, gain: f32, peak: f32) -> f32 {
        let mut nf = f32::powf(10.0, (pregain + gain) / 20.0);
        if nf * peak > 1.0 {
            nf = 1.0 / peak;
        }
        nf
    }
}

/// ReplayGain values collected from the tags of a local file.
#[derive(Default)]
pub struct ReplayGainTags {
    track_gain_db: Option<f32>,
    track_peak: Option<f32>,
    album_gain_db: Option<f32>,
    album_peak: Option<f32>,
}

impl ReplayGainTags {
    pub fn read(&mut self, revision: &MetadataRevision) {
        for tag in revision.tags() {
            // Symphonia only maps some spellings to standard keys (e.g. not
            // lowercase ID3 `TXXX` frames or MP4 freeform atoms), so fall back
            // to matching the raw key.
            let key = tag.key.to_ascii_lowercase();
            let slot = match tag.std_key {
                Some(StandardTagKey::ReplayGainTrackGain) => &mut self.track_gain_db,
                Some(StandardTagKey::ReplayGainTrackPeak) => &mut self.track_peak,
                Some(StandardTagKey::ReplayGainAlbumGain) => &mut self.album_gain_db,
                Some(StandardTagKey::ReplayGainAlbumPeak) => &mut self.album_peak,
                _ if key.ends_with("replaygain_track_gain") => &mut self.track_gain_db,
                _ if key.ends_with("replaygain_track_peak") => &mut self.track_peak,
                _ if key.ends_with("replaygain_album_gain") => &mut self.album_gain_db,
                _ if key.ends_with("replaygain_album_peak") => &mut self.album_peak,
                _ => continue,
            };
            if let Some(value) = parse_replay_gain_value(&tag.value.to_string()) {
                *slot = Some(value);
            }
        }
    }

    /// Returns `None` if the file has no ReplayGain tags at all.  If only one of
    /// the track or album values is present, it's used for both levels.
    pub fn into_normalization_data(self) -> Option<NormalizationData> {
        let track_gain = self.track_gain_db.or(self.album_gain_db)?;
        let album_gain = self.album_gain_db.unwrap_or(track_gain);
        let track_peak = self.track_peak.or(self.album_peak).unwrap_or(1.0);
        let album_peak = self.album_peak.unwrap_or(track_peak);
        Some(NormalizationData::from_replay_gain(
            track_gain, track_peak, album_gain, album_peak,
        ))
    }
}

/// Parses values like `-6.54 dB`, `+1.2dB` or `0.988547`.
fn parse_replay_gain_value(value: &str) -> Option<f32> {
    let number = value.split_whitespace().next()?;
    let number = number.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    number.parse::<f32>().ok().filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replay_gain_values() {
        assert_eq!(parse_replay_gain_value("-6.54 dB"), Some(-6.54));
        assert_eq!(parse_replay_gain_value("+1.20dB"), Some(1.2));
        assert_eq!(parse_replay_gain_value("0.988547"), Some(0.988547));
        assert_eq!(parse_replay_gain_value("garbage"), None);
        assert_eq!(parse_replay_gain_value(""), None);
    }

    #[test]
    fn missing_replay_gain_means_no_normalization() {
        assert!(ReplayGainTags::default().into_normalization_data().is_none());
    }

    #[test]
    fn album_gain_falls_back_to_track_gain() {
        let tags = ReplayGainTags {
            track_gain_db: Some(-8.0),
            track_peak: Some(0.5),
            ..Default::default()
        };
        let data = tags.into_normalization_data().unwrap();
        let track = data.factor_for_level(NormalizationLevel::Track, 0.0);
        let album = data.factor_for_level(NormalizationLevel::Album, 0.0);
        assert_eq!(track, album);
        assert!((track - f32::powf(10.0, -4.0 / 20.0)).abs() < 1e-6);
    }
}
