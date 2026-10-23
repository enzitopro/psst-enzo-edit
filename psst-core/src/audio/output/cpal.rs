use std::{sync::Arc, time::Duration};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{unbounded, Receiver, Sender};
use num_traits::Pow;
use parking_lot::Mutex;

use crate::{
    actor::{Act, Actor, ActorHandle},
    audio::{
        output::{AudioOutput, AudioSink},
        resample::ResamplingQuality,
        source::{AudioSource, Empty, ResampledSource, StereoMappedSource},
    },
    error::Error,
};

pub struct CpalOutput {
    _handle: ActorHandle<StreamMsg>,
    sink: CpalSink,
}

impl CpalOutput {
    pub fn open() -> Result<Self, Error> {
        // Open the default output device.
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(cpal::DefaultStreamConfigError::DeviceNotAvailable)?;

        if let Ok(name) = device.name() {
            log::info!("using audio device: {name:?}");
        }

        // Get the default device config, so we know what sample format and sample rate
        // the device supports.
        let supported = Self::preferred_output_config(&device)?;

        // Unbounded: only the audio callback drains it, and that doesn't run while
        // paused, so a bounded one filled up with volume changes and blocked the
        // player for good, which then never took the resume.
        let (callback_send, callback_recv) = unbounded();

        let handle = Stream::spawn_with_default_cap("audio_output", {
            let config = supported.config();
            // TODO: Support additional sample formats.
            move |this| Stream::open(device, config, callback_recv, this).unwrap()
        });
        // Start watching for audio device changes.
        let _ = handle.send(StreamMsg::CheckDevice);
        let sink = CpalSink {
            channel_count: supported.channels(),
            sample_rate: supported.sample_rate(),
            stream_send: handle.sender(),
            callback_send,
        };

        Ok(Self {
            _handle: handle,
            sink,
        })
    }

    fn preferred_output_config(
        device: &cpal::Device,
    ) -> Result<cpal::SupportedStreamConfig, Error> {
        const PREFERRED_SAMPLE_FORMAT: cpal::SampleFormat = cpal::SampleFormat::F32;
        const PREFERRED_SAMPLE_RATE: cpal::SampleRate = cpal::SampleRate(44_100);
        const PREFERRED_CHANNELS: cpal::ChannelCount = 2;

        for s in device.supported_output_configs()? {
            let rates = s.min_sample_rate()..=s.max_sample_rate();
            if s.channels() == PREFERRED_CHANNELS
                && s.sample_format() == PREFERRED_SAMPLE_FORMAT
                && rates.contains(&PREFERRED_SAMPLE_RATE)
            {
                return Ok(s.with_sample_rate(PREFERRED_SAMPLE_RATE));
            }
        }

        Ok(device.default_output_config()?)
    }
}

impl AudioOutput for CpalOutput {
    type Sink = CpalSink;

    fn sink(&self) -> Self::Sink {
        self.sink.clone()
    }
}

#[derive(Clone)]
pub struct CpalSink {
    channel_count: cpal::ChannelCount,
    sample_rate: cpal::SampleRate,
    callback_send: Sender<CallbackMsg>,
    stream_send: Sender<StreamMsg>,
}

impl CpalSink {
    fn send_to_callback(&self, msg: CallbackMsg) {
        if self.callback_send.send(msg).is_err() {
            log::error!("output stream actor is dead");
        }
    }

    fn send_to_stream(&self, msg: StreamMsg) {
        if self.stream_send.send(msg).is_err() {
            log::error!("output stream actor is dead");
        }
    }
}

impl AudioSink for CpalSink {
    fn channel_count(&self) -> usize {
        self.channel_count as usize
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate.0
    }

    fn set_volume(&self, volume: f32) {
        self.send_to_callback(CallbackMsg::SetVolume(volume));
    }

    fn play(&self, source: impl AudioSource) {
        self.send_to_callback(CallbackMsg::PlaySource(Box::new(source)));
    }

    fn pause(&self) {
        self.send_to_stream(StreamMsg::Pause);
        self.send_to_callback(CallbackMsg::Pause);
    }

    fn resume(&self) {
        self.send_to_stream(StreamMsg::Resume);
        self.send_to_callback(CallbackMsg::Resume);
    }

    fn stop(&self) {
        self.play(Empty);
        self.pause();
    }

    fn close(&self) {
        self.send_to_stream(StreamMsg::Close);
    }
}

/// How often to check whether the default output device changed, e.g. after
/// plugging in headphones.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// The output stream follows the system's default device: when it changes or
/// disappears, a new stream is opened on the new default, and playback goes
/// on there.  The playing source, volume and play state live in `Shared`, so
/// they carry over to the new stream.
struct Stream {
    stream: cpal::Stream,
    device_name: Option<String>,
    shared: Arc<Mutex<StreamCallback>>,
    /// The format the sink advertises, and the decoder side produces.  It
    /// stays the same for the whole session, other devices get converted.
    channels: cpal::ChannelCount,
    sample_rate: cpal::SampleRate,
    stream_send: Sender<StreamMsg>,
    playing: bool,
    /// Bumped with every new stream, so late errors of a replaced stream
    /// don't trigger another switch.
    generation: u64,
}

impl Stream {
    fn open(
        device: cpal::Device,
        config: cpal::StreamConfig,
        callback_recv: Receiver<CallbackMsg>,
        stream_send: Sender<StreamMsg>,
    ) -> Result<Self, Error> {
        let shared = Arc::new(Mutex::new(StreamCallback {
            callback_recv,
            source: Box::new(Empty),
            volume: 1.0, // We start with the full volume.
            state: CallbackState::Paused,
        }));
        let device_name = device.name().ok();
        log::info!("opening output stream: {config:?}");
        let stream = Self::build_stream(
            &device,
            &config,
            &shared,
            config.channels,
            config.sample_rate,
            &stream_send,
            0,
        )?;
        Ok(Self {
            stream,
            device_name,
            shared,
            channels: config.channels,
            sample_rate: config.sample_rate,
            stream_send,
            playing: false,
            generation: 0,
        })
    }

    fn build_stream(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        shared: &Arc<Mutex<StreamCallback>>,
        channels: cpal::ChannelCount,
        sample_rate: cpal::SampleRate,
        stream_send: &Sender<StreamMsg>,
        generation: u64,
    ) -> Result<cpal::Stream, Error> {
        let error_send = stream_send.clone();
        let on_error = move |err: cpal::StreamError| {
            log::error!("audio output error: {err}");
            if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                let _ = error_send.try_send(StreamMsg::DeviceLost(generation));
            }
        };
        let stream = if config.channels == channels && config.sample_rate == sample_rate {
            let shared = Arc::clone(shared);
            device.build_output_stream(
                config,
                move |output: &mut [f32], _| shared.lock().write_samples(output),
                on_error,
                None,
            )?
        } else {
            // The device wants another format than the one the playback was
            // set up for, convert on the fly.
            log::info!(
                "converting audio from {} Hz, {} ch to {} Hz, {} ch",
                sample_rate.0,
                channels,
                config.sample_rate.0,
                config.channels
            );
            let reader = SharedSource {
                shared: Arc::clone(shared),
                channels: channels as usize,
                sample_rate: sample_rate.0,
            };
            let resampled = ResampledSource::new(
                reader,
                config.sample_rate.0,
                ResamplingQuality::SincMediumQuality,
            );
            let mut converted: Box<dyn AudioSource> = if config.channels != channels {
                Box::new(StereoMappedSource::new(resampled, config.channels as usize))
            } else {
                Box::new(resampled)
            };
            device.build_output_stream(
                config,
                move |output: &mut [f32], _| {
                    let written = converted.write(output);
                    output[written..].iter_mut().for_each(|s| *s = 0.0);
                },
                on_error,
                None,
            )?
        };
        Ok(stream)
    }

    /// Moves playback to the current default device, if it's another one than
    /// we're using, or if ours is gone (`lost`).
    fn follow_default_device(&mut self, lost: bool) {
        let Some(device) = cpal::default_host().default_output_device() else {
            if lost {
                log::warn!("no audio output device available");
            }
            return;
        };
        let name = device.name().ok();
        if !lost && name == self.device_name {
            return;
        }
        log::info!("switching audio output to {name:?}");
        // Keep the old stream from pulling samples meanwhile.
        let _ = self.stream.pause();
        let generation = self.generation + 1;
        let result = CpalOutput::preferred_output_config(&device).and_then(|supported| {
            Self::build_stream(
                &device,
                &supported.config(),
                &self.shared,
                self.channels,
                self.sample_rate,
                &self.stream_send,
                generation,
            )
        });
        match result {
            Ok(stream) => {
                if self.playing {
                    if let Err(err) = stream.play() {
                        log::error!("failed to start stream: {err}");
                    }
                }
                self.stream = stream;
                self.device_name = name;
                self.generation = generation;
            }
            Err(err) => {
                // Try again on the next check.
                log::error!("failed to open audio device {name:?}: {err}");
                if self.playing && !lost {
                    let _ = self.stream.play();
                }
            }
        }
    }
}

impl Actor for Stream {
    type Message = StreamMsg;
    type Error = Error;

    fn handle(&mut self, msg: Self::Message) -> Result<Act<Self>, Self::Error> {
        match msg {
            StreamMsg::Pause => {
                log::debug!("pausing audio output stream");
                self.playing = false;
                if let Err(err) = self.stream.pause() {
                    log::error!("failed to stop stream: {err}");
                }
            }
            StreamMsg::Resume => {
                log::debug!("resuming audio output stream");
                self.playing = true;
                if let Err(err) = self.stream.play() {
                    log::error!("failed to start stream: {err}");
                }
            }
            StreamMsg::Close => {
                log::debug!("closing audio output stream");
                let _ = self.stream.pause();
                return Ok(Act::Shutdown);
            }
            StreamMsg::DeviceLost(generation) if generation == self.generation => {
                self.follow_default_device(true)
            }
            StreamMsg::DeviceLost(_) => {}
            StreamMsg::CheckDevice => self.follow_default_device(false),
        }
        Ok(Act::WaitOr {
            timeout: DEVICE_CHECK_INTERVAL,
            timeout_msg: StreamMsg::CheckDevice,
        })
    }
}

enum StreamMsg {
    Pause,
    Resume,
    Close,
    /// The device of the stream of this generation went away.
    DeviceLost(u64),
    /// Periodic check for a new default device.
    CheckDevice,
}

/// Reads from the shared callback state, for streams that need conversion.
struct SharedSource {
    shared: Arc<Mutex<StreamCallback>>,
    channels: usize,
    sample_rate: u32,
}

impl AudioSource for SharedSource {
    fn write(&mut self, output: &mut [f32]) -> usize {
        self.shared.lock().write_samples(output);
        output.len()
    }

    fn channel_count(&self) -> usize {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

enum CallbackMsg {
    PlaySource(Box<dyn AudioSource>),
    SetVolume(f32),
    Pause,
    Resume,
}

enum CallbackState {
    Playing,
    Paused,
}

struct StreamCallback {
    callback_recv: Receiver<CallbackMsg>,
    source: Box<dyn AudioSource>,
    state: CallbackState,
    volume: f32,
}

impl StreamCallback {
    fn write_samples(&mut self, output: &mut [f32]) {
        // Process any pending data messages.
        while let Ok(msg) = self.callback_recv.try_recv() {
            match msg {
                CallbackMsg::PlaySource(src) => {
                    self.source = src;
                }
                CallbackMsg::SetVolume(volume) => {
                    self.volume = volume;
                }
                CallbackMsg::Pause => {
                    self.state = CallbackState::Paused;
                }
                CallbackMsg::Resume => {
                    self.state = CallbackState::Playing;
                }
            }
        }

        let written = if matches!(self.state, CallbackState::Playing) {
            // Write out as many samples as possible from the audio source to the
            // output buffer.
            let written = self.source.write(output);

            // Apply scaled global volume level.
            let scaled_volume = self.volume.pow(4);
            output[..written]
                .iter_mut()
                .for_each(|s| *s *= scaled_volume);

            written
        } else {
            0
        };

        // Mute any remaining samples.
        output[written..].iter_mut().for_each(|s| *s = 0.0);
    }
}

impl From<cpal::DefaultStreamConfigError> for Error {
    fn from(err: cpal::DefaultStreamConfigError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<cpal::SupportedStreamConfigsError> for Error {
    fn from(err: cpal::SupportedStreamConfigsError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<cpal::BuildStreamError> for Error {
    fn from(err: cpal::BuildStreamError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<cpal::PlayStreamError> for Error {
    fn from(err: cpal::PlayStreamError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<cpal::PauseStreamError> for Error {
    fn from(err: cpal::PauseStreamError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;
    use crate::audio::output::{AudioOutput, AudioSink};

    /// Needs an audio device, so not on CI.
    #[test]
    #[ignore]
    fn volume_changes_while_paused_dont_block() {
        let output = CpalOutput::open().expect("audio output");
        let sink = output.sink();
        sink.set_volume(0.0);
        sink.pause();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            // A volume slider dragged while paused.
            for step in 0..200 {
                sink.set_volume(step as f32 / 2000.0);
            }
            sink.resume();
            done.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}
