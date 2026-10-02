//! The part of obs-irl-source that needs neither libobs nor FFmpeg: the audio
//! jitter buffer, PTS discontinuity repair, the playback-speed controller,
//! output-clock arithmetic, video pacing, the demuxer option table, config
//! derivation and the stats field table.
//!
//! Everything here is plain data in, plain data out. Time values are passed
//! in as parameters (nanoseconds for the OBS domain, microseconds for the
//! FFmpeg domain) rather than read from a clock, so tests are deterministic
//! and the two domains cannot be mixed by accident.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod arrival;
pub mod audio_buffer;
pub mod audio_hold;
pub mod config;
pub mod consts;
pub mod dsp;
pub mod pacing;
pub mod playout;
pub mod pts_repair;
mod rescale;
pub mod speed;
pub mod stats;
pub mod timing;
pub mod url_opts;
pub mod video_delay;
pub mod video_time;
mod window;

pub use audio_buffer::{AudioBuffer, BufferState};
pub use audio_hold::{AudioHold, HoldChange};
pub use config::{HwDecode, Watermarks};
pub use dsp::LastSample;
pub use pacing::{DueVerdict, PacingQueue};
pub use playout::PlayoutMapping;
pub use pts_repair::{PtsAction, PtsRepair, Verdict};
pub use speed::{
    DrainWatch, SpeedCarry, SpeedController, SpeedInputs, SpeedTrim, StuckReport, catchup_speed_max,
};
pub use stats::{StatKind, StatValue, StatsSnapshot};
pub use timing::OutputClock;
pub use url_opts::awaits_caller;
pub use video_delay::{DelayRaise, DelayRelax, RampStep, VideoDelay};
