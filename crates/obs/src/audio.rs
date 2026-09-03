//! Audio output.

use core::marker::PhantomData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Float,
}

impl AudioFormat {
    #[must_use]
    pub fn to_sys(self) -> obs_sys::audio_format {
        match self {
            // Interleaved 32-bit float: the only format the plugin submits,
            // because swresample always converts to it first.
            Self::Float => obs_sys::audio_format::AUDIO_FORMAT_FLOAT,
        }
    }
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

    #[must_use]
    pub fn to_sys(self) -> obs_sys::speaker_layout {
        use obs_sys::speaker_layout as S;
        match self {
            Self::Unknown => S::SPEAKERS_UNKNOWN,
            Self::Mono => S::SPEAKERS_MONO,
            Self::Stereo => S::SPEAKERS_STEREO,
            Self::Stereo2Point1 => S::SPEAKERS_2POINT1,
            Self::Quad4Point0 => S::SPEAKERS_4POINT0,
            Self::Quad4Point1 => S::SPEAKERS_4POINT1,
            Self::Surround5Point1 => S::SPEAKERS_5POINT1,
            Self::Surround7Point1 => S::SPEAKERS_7POINT1,
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
    #[must_use]
    pub fn interleaved(
        data: &'a [u8],
        frames: u32,
        speakers: SpeakerLayout,
        samples_per_sec: u32,
        format: AudioFormat,
        timestamp_ns: u64,
    ) -> Self {
        // Interleaved formats put everything in plane 0; the rest stay NULL,
        // which is what libobs expects (it reads one plane for a packed
        // format). `'a` keeps the buffer alive until obs_source_output_audio,
        // which is where libobs copies.
        let mut planes = [core::ptr::null(); obs_sys::MAX_AV_PLANES];
        planes[0] = data.as_ptr();

        Self {
            inner: obs_sys::obs_source_audio {
                data: planes,
                frames,
                speakers: speakers.to_sys(),
                format: format.to_sys(),
                samples_per_sec,
                timestamp: timestamp_ns,
            },
            _data: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn as_sys(&self) -> &obs_sys::obs_source_audio {
        &self.inner
    }
}
