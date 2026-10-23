use crate::audio::source::AudioSource;

#[cfg(feature = "cpal")]
pub mod cpal;

#[cfg(feature = "cpal")]
pub type DefaultAudioOutput = cpal::CpalOutput;

pub type DefaultAudioSink = <DefaultAudioOutput as AudioOutput>::Sink;

pub trait AudioOutput {
    type Sink: AudioSink;

    fn sink(&self) -> Self::Sink;
}

pub trait AudioSink {
    fn channel_count(&self) -> usize;
    fn sample_rate(&self) -> u32;
    fn set_volume(&self, volume: f32);
    fn play(&self, source: impl AudioSource);
    /// Plays `source`, fading the playing one out over `duration` as it fades in.
    fn crossfade(&self, source: impl AudioSource, duration: std::time::Duration);
    fn pause(&self);
    fn resume(&self);
    fn stop(&self);
    fn close(&self);
}
