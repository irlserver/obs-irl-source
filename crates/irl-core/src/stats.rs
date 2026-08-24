//! The stats surface, defined once: the proc-handler declaration, the
//! calldata writer and the websocket vendor's copy loop all iterate
//! [`FIELDS`], so a new stat is a one-line change.

/// calldata type of a stat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    /// `long long`.
    Int,
    /// `double`.
    Float,
    /// `bool`.
    Bool,
}

/// A value of one stat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatValue {
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// Bool.
    Bool(bool),
}

/// The 27 stat fields in proc-declaration order.
pub const FIELDS: &[(&str, StatKind)] = &[
    ("buffer_fill_ms", StatKind::Int),
    ("current_speed", StatKind::Float),
    ("adaptive_latency_control", StatKind::Bool),
    ("reconnecting", StatKind::Bool),
    ("total_audio_frames", StatKind::Int),
    ("total_video_frames", StatKind::Int),
    ("pts_repairs", StatKind::Int),
    ("pts_normalizations", StatKind::Int),
    ("pts_interpolations", StatKind::Int),
    ("pts_resets", StatKind::Int),
    ("pts_last_gap_ms", StatKind::Int),
    ("pts_max_gap_ms", StatKind::Int),
    ("silence_insertions", StatKind::Int),
    ("audio_underruns", StatKind::Int),
    ("audio_resync_skipped_chunks", StatKind::Int),
    ("audio_hidden_trimmed_chunks", StatKind::Int),
    ("audio_quality_events", StatKind::Int),
    ("audio_output_restarts", StatKind::Int),
    ("obs_lead_ms", StatKind::Int),
    ("audio_decoder_flushes", StatKind::Int),
    ("video_corrupt_frames", StatKind::Int),
    ("video_corrupt_held", StatKind::Int),
    ("video_lead_ms", StatKind::Int),
    ("video_lead_excess", StatKind::Int),
    ("stream_delay_ms", StatKind::Int),
    ("low_latency_audio", StatKind::Bool),
    ("reconnect_count", StatKind::Int),
];

/// A snapshot of every stat, in [`FIELDS`] order.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StatsSnapshot {
    /// Jitter buffer fill.
    pub buffer_fill_ms: i64,
    /// Smoothed playback speed.
    pub current_speed: f64,
    /// Adaptive latency control enabled.
    pub adaptive_latency_control: bool,
    /// Reconnecting.
    pub reconnecting: bool,
    /// Audio frames decoded this connection.
    pub total_audio_frames: i64,
    /// Video frames decoded this connection.
    pub total_video_frames: i64,
    /// PTS repairs.
    pub pts_repairs: i64,
    /// PTS normalizations.
    pub pts_normalizations: i64,
    /// PTS interpolations.
    pub pts_interpolations: i64,
    /// PTS resets.
    pub pts_resets: i64,
    /// Last PTS gap.
    pub pts_last_gap_ms: i64,
    /// Largest PTS gap.
    pub pts_max_gap_ms: i64,
    /// Silence insertions.
    pub silence_insertions: i64,
    /// Underruns.
    pub audio_underruns: i64,
    /// Chunks skipped on resync.
    pub audio_resync_skipped_chunks: i64,
    /// Chunks trimmed silently.
    pub audio_hidden_trimmed_chunks: i64,
    /// Quality events.
    pub audio_quality_events: i64,
    /// Output clock restarts.
    pub audio_output_restarts: i64,
    /// Queued audio lead ahead of wall clock.
    pub obs_lead_ms: i64,
    /// Audio decoder flushes.
    pub audio_decoder_flushes: i64,
    /// Corrupt video frames seen.
    pub video_corrupt_frames: i64,
    /// Corrupt video frames held back.
    pub video_corrupt_held: i64,
    /// Video lead.
    pub video_lead_ms: i64,
    /// Video lead excess events.
    pub video_lead_excess: i64,
    /// Estimated end-to-end delay.
    pub stream_delay_ms: i64,
    /// Low latency audio enabled.
    pub low_latency_audio: bool,
    /// Reconnects.
    pub reconnect_count: i64,
}

impl StatsSnapshot {
    /// Values in [`FIELDS`] order.
    pub fn values(&self) -> [StatValue; FIELDS.len()] {
        todo!("W1-C")
    }
}

/// The `proc_handler_add` declaration string,
/// `void get_stats(out int buffer_fill_ms, out float current_speed, ...)`.
pub fn proc_declaration() -> String {
    todo!("W1-C")
}
