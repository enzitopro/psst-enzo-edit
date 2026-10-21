use std::{io, time::Duration};

use symphonia::{
    core::{
        audio::{Channels, SampleBuffer, SignalSpec},
        codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL},
        conv::ConvertibleSample,
        errors::Error as SymphoniaError,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
        io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
        units::{TimeBase, TimeStamp},
    },
    default::{
        codecs::{MpaDecoder, VorbisDecoder},
        formats::{MpaReader, OggReader},
    },
};

use crate::{
    audio::normalize::{NormalizationData, ReplayGainTags},
    error::Error,
    util::FileWithConstSize,
};

pub enum AudioCodecFormat {
    Mp3,
    OggVorbis,
}

impl AudioCodecFormat {
    fn format_reader(
        &self,
        mss: MediaSourceStream,
    ) -> Result<Box<dyn FormatReader>, SymphoniaError> {
        match self {
            Self::Mp3 => Ok(Box::new(MpaReader::try_new(
                mss,
                &FormatOptions::default(),
            )?)),
            Self::OggVorbis => Ok(Box::new(OggReader::try_new(
                mss,
                &FormatOptions::default(),
            )?)),
        }
    }

    fn decoder(&self, codec_params: &CodecParameters) -> Result<Box<dyn Decoder>, SymphoniaError> {
        match self {
            Self::Mp3 => Ok(Box::new(MpaDecoder::try_new(
                codec_params,
                &DecoderOptions::default(),
            )?)),
            Self::OggVorbis => Ok(Box::new(VorbisDecoder::try_new(
                codec_params,
                &DecoderOptions::default(),
            )?)),
        }
    }
}

pub struct AudioDecoder {
    track_id: u32, // Internal track index.
    decoder: Box<dyn Decoder>,
    format: Box<dyn FormatReader>,
    // Parameters of the selected track as reported by the container.  Not all
    // decoders carry every field over into their own parameters.
    track_params: CodecParameters,
    spec: SignalSpec,
}

impl AudioDecoder {
    pub fn new<T>(input: T, codec: AudioCodecFormat) -> Result<Self, Error>
    where
        T: io::Read + io::Seek + Send + Sync + 'static,
    {
        let mss = MediaSourceStream::new(
            Box::new(FileWithConstSize::new(input)),
            MediaSourceStreamOptions::default(),
        );
        let format = codec.format_reader(mss)?;
        let track = format
            .default_track()
            .ok_or(SymphoniaError::Unsupported("file contains no audio track"))?;
        let decoder = codec.decoder(&track.codec_params)?;
        let spec = known_signal_spec(decoder.codec_params(), &track.codec_params).ok_or(
            SymphoniaError::Unsupported("unknown sample rate or channel count"),
        )?;

        Ok(Self {
            track_id: track.id,
            track_params: track.codec_params.clone(),
            decoder,
            format,
            spec,
        })
    }

    /// Open a file of unknown format, picking the container and codec from all
    /// the ones Symphonia was built with.  Used for local files.  Also returns
    /// the ReplayGain data found in the file tags, if there is any.
    pub fn probe<T>(
        input: T,
        extension: Option<&str>,
    ) -> Result<(Self, Option<NormalizationData>), Error>
    where
        T: io::Read + io::Seek + Send + Sync + 'static,
    {
        let mss = MediaSourceStream::new(
            Box::new(FileWithConstSize::new(input)),
            MediaSourceStreamOptions::default(),
        );
        let mut hint = Hint::new();
        if let Some(ext) = extension {
            hint.with_extension(ext);
        }
        let mut probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        // Tags can live both in front of the container (ID3v2 before an MP3)
        // and inside of it (Vorbis comments, MP4 atoms).
        let mut replay_gain = ReplayGainTags::default();
        if let Some(metadata) = probed.metadata.get() {
            if let Some(revision) = metadata.current() {
                replay_gain.read(revision);
            }
        }
        if let Some(revision) = probed.format.metadata().current() {
            replay_gain.read(revision);
        }

        let format = probed.format;
        // Skip over non-audio tracks, like embedded cover art.
        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(SymphoniaError::Unsupported("file contains no audio track"))?;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;
        let track_id = track.id;
        let track_params = track.codec_params.clone();
        let spec = known_signal_spec(decoder.codec_params(), &track_params);

        let mut decoder = Self {
            track_id,
            track_params,
            decoder,
            format,
            // Replaced below if the container doesn't tell.
            spec: spec.unwrap_or(SignalSpec {
                rate: 0,
                channels: Channels::empty(),
            }),
        };
        if spec.is_none() {
            decoder.spec = decoder.discover_signal_spec()?;
        }
        Ok((decoder, replay_gain.into_normalization_data()))
    }

    /// Some containers (e.g. AAC in MP4) only reveal the channel layout once
    /// the first packet is decoded.  Decode it, then start over.
    fn discover_signal_spec(&mut self) -> Result<SignalSpec, Error> {
        let spec = loop {
            let packet = self.format.next_packet()?;
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => break *decoded.spec(),
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(err) => return Err(err.into()),
            }
        };
        self.format.seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: 0,
                track_id: self.track_id,
            },
        )?;
        self.decoder.reset();
        Ok(spec)
    }

    pub fn codec_params(&self) -> &CodecParameters {
        self.decoder.codec_params()
    }

    pub fn signal_spec(&self) -> SignalSpec {
        self.spec
    }

    pub fn time_base(&self) -> TimeBase {
        self.codec_params()
            .time_base
            .or(self.track_params.time_base)
            .unwrap_or_else(|| TimeBase::new(1, self.signal_spec().rate))
    }

    pub fn seek(&mut self, time: Duration) -> Result<TimeStamp, Error> {
        let seeked_to = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: time.as_secs_f64().into(),
                track_id: Some(self.track_id),
            },
        )?;
        Ok(seeked_to.actual_ts)
    }

    /// Read a next packet of audio from this decoder.  Returns `None` in case
    /// of EOF or internal error.
    pub fn read_packet<S>(&mut self, samples: &mut SampleBuffer<S>) -> Option<TimeStamp>
    where
        S: ConvertibleSample,
    {
        loop {
            // Demux an encoded packet from the media format.
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(io)) if io.kind() == io::ErrorKind::UnexpectedEof => {
                    return None; // End of this stream.
                }
                Err(err) => {
                    log::error!("format error: {err}");
                    return None; // We cannot recover from format errors, quit.
                }
            };
            while !self.format.metadata().is_latest() {
                // Consume any new metadata that has been read since the last
                // packet.
            }
            // If the packet does not belong to the selected track, skip over it.
            if packet.track_id() != self.track_id {
                continue;
            }
            // Decode the packet into an audio buffer.
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    // Interleave the samples into the buffer.
                    samples.copy_interleaved_ref(decoded);
                    return Some(packet.ts());
                }
                Err(SymphoniaError::IoError(err)) => {
                    // The packet failed to decode due to an IO error, skip the packet.
                    log::error!("io decode error: {err}");
                    continue;
                }
                Err(SymphoniaError::DecodeError(err)) => {
                    // The packet failed to decode due to invalid data, skip the packet.
                    log::error!("decode error: {err}");
                    continue;
                }
                Err(err) => {
                    log::error!("fatal decode error: {err}");
                    return None;
                }
            };
        }
    }
}

fn known_signal_spec(decoder: &CodecParameters, track: &CodecParameters) -> Option<SignalSpec> {
    Some(SignalSpec {
        rate: decoder.sample_rate.or(track.sample_rate)?,
        channels: decoder.channels.or(track.channels)?,
    })
}

impl<T> MediaSource for FileWithConstSize<T>
where
    T: io::Read + io::Seek + Send + Sync,
{
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len())
    }
}

impl From<SymphoniaError> for Error {
    fn from(err: SymphoniaError) -> Error {
        Error::AudioDecodingError(Box::new(err))
    }
}
