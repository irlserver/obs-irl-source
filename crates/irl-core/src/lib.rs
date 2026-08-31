//! The part of obs-irl-source that needs neither libobs nor FFmpeg: the audio
//! jitter buffer, PTS discontinuity repair, the playback-speed controller,
//! output-clock arithmetic, video pacing, the demuxer option table, config
//! derivation and the stats field table.
//!
//! Everything here is plain data in, plain data out, so it is unit-tested
//! against the thresholds the C plugin was tuned with. Time values are passed
//! in as parameters (nanoseconds for the OBS domain, microseconds for the
//! FFmpeg domain) rather than read from a clock, so tests are deterministic
//! and the two domains cannot be mixed by accident.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod audio_buffer;
pub mod config;
pub mod consts;
pub mod dsp;
pub mod pacing;
pub mod pts_repair;
mod rescale;
pub mod speed;
pub mod stats;
pub mod timing;
pub mod url_opts;
pub mod video_time;

pub use audio_buffer::{AudioBuffer, BufferState};
pub use config::{HwDecode, Watermarks};
pub use dsp::LastSample;
pub use pacing::{DueVerdict, PacedFrame, PacingQueue};
pub use pts_repair::{PtsAction, PtsRepair, Verdict};
pub use speed::{
    DrainWatch, SpeedCarry, SpeedController, SpeedInputs, SpeedTrim, StuckReport, catchup_speed_max,
};
pub use stats::{StatKind, StatValue, StatsSnapshot};
