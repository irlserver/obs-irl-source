//! Audio output.

use core::marker::PhantomData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Float,
}

/// `enum speaker_layout`. [`SpeakerLayout::from_channels`] reproduces the C
/// plugin's `(enum speaker_layout)channels` cast for the values libobs
/// defines (1, 2, 3, 4, 5, 6, 8) and yields `Unknown` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerLayout {
    Unknown,
    Mono,
    Stereo,
    Stereo2Point1,
    Quad4Point0,
    Quad4Point1,
    Surround5Point1,
    Surround7Point1,
}

impl SpeakerLayout {
    pub fn from_channels(channels: u32) -> Self {
        match channels {
            1 => Self::Mono,
            2 => Self::Stereo,
            3 => Self::Stereo2Point1,
            4 => Self::Quad4Point0,
            5 => Self::Quad4Point1,
            6 => Self::Surround5Point1,
            8 => Self::Surround7Point1,
            _ => Self::Unknown,
        }
    }
}

/// An `obs_source_audio` borrowing one interleaved buffer.
pub struct AudioFrame<'a> {
    inner: obs_sys::obs_source_audio,
    _data: PhantomData<&'a [u8]>,
}

impl<'a> AudioFrame<'a> {
    /// Interleaved samples in `data` (`frames * channels * sample_size` bytes).
    pub fn interleaved(
        data: &'a [u8],
        frames: u32,
        speakers: SpeakerLayout,
        samples_per_sec: u32,
        format: AudioFormat,
        timestamp_ns: u64,
    ) -> Self {
        let _ = (data, frames, speakers, samples_per_sec, format, timestamp_ns);
        todo!("W1-A")
    }

    #[doc(hidden)]
    pub fn as_sys(&self) -> &obs_sys::obs_source_audio {
        &self.inner
    }
}
