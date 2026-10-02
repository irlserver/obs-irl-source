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

/// The stat fields in proc-declaration order.
pub const FIELDS: &[(&str, StatKind)] = &[
    ("buffer_fill_ms", StatKind::Int),
    ("current_speed", StatKind::Float),
    ("total_audio_frames", StatKind::Int),
    ("total_video_frames", StatKind::Int),
    ("pts_max_gap_ms", StatKind::Int),
    ("audio_underruns", StatKind::Int),
    ("audio_output_restarts", StatKind::Int),
    ("video_delay_ms", StatKind::Int),
    ("av_skew_ms", StatKind::Int),
    ("audio_hold_ms", StatKind::Int),
    ("reconnecting", StatKind::Bool),
];

/// A snapshot of every stat, in [`FIELDS`] order.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StatsSnapshot {
    /// Jitter buffer fill.
    pub buffer_fill_ms: i64,
    /// Smoothed playback speed.
    pub current_speed: f64,
    /// Audio frames decoded this connection.
    pub total_audio_frames: i64,
    /// Video frames decoded this connection.
    pub total_video_frames: i64,
    /// Largest audio PTS gap this connection.
    pub pts_max_gap_ms: i64,
    /// Underruns.
    pub audio_underruns: i64,
    /// Output clock restarts.
    pub audio_output_restarts: i64,
    /// Standing delay added to the video schedule so late-arriving video can
    /// still be paced; lip sync is off by this much.
    pub video_delay_ms: i64,
    /// Video PTS minus audio PTS of what last reached the plugin: how the
    /// sender stamps the two streams against each other, before any buffering
    /// here. Near zero for a healthy sender; negative when video trails audio.
    pub av_skew_ms: i64,
    /// How much longer than Target Buffer audio is held so that video which
    /// arrives behind it is in hand when the two are due
    /// (`irl_core::audio_hold`).
    pub audio_hold_ms: i64,
    /// Reconnecting.
    pub reconnecting: bool,
}

impl StatKind {
    /// The calldata type name as it appears in a proc declaration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
        }
    }
}

impl StatsSnapshot {
    /// Values in [`FIELDS`] order.
    pub fn values(&self) -> [StatValue; FIELDS.len()] {
        [
            StatValue::Int(self.buffer_fill_ms),
            StatValue::Float(self.current_speed),
            StatValue::Int(self.total_audio_frames),
            StatValue::Int(self.total_video_frames),
            StatValue::Int(self.pts_max_gap_ms),
            StatValue::Int(self.audio_underruns),
            StatValue::Int(self.audio_output_restarts),
            StatValue::Int(self.video_delay_ms),
            StatValue::Int(self.av_skew_ms),
            StatValue::Int(self.audio_hold_ms),
            StatValue::Bool(self.reconnecting),
        ]
    }

    /// The named stat, for a caller that wants one field by name (the
    /// websocket vendor's copy loop walks [`FIELDS`] instead).
    pub fn get(&self, name: &str) -> Option<StatValue> {
        let index = FIELDS.iter().position(|(field, _)| *field == name)?;
        Some(self.values()[index])
    }
}

/// The `proc_handler_add` declaration string,
/// `void get_stats(out int buffer_fill_ms, out float current_speed, ...)`.
pub fn proc_declaration() -> String {
    let mut decl = String::from("void get_stats(");
    for (i, (name, kind)) in FIELDS.iter().enumerate() {
        if i > 0 {
            decl.push_str(", ");
        }
        decl.push_str("out ");
        decl.push_str(kind.as_str());
        decl.push(' ');
        decl.push_str(name);
    }
    decl.push(')');
    decl
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What scripts and obs-websocket clients bind against.
    const DECLARATION: &str = "void get_stats(out int buffer_fill_ms, \
out float current_speed, \
out int total_audio_frames, out int total_video_frames, \
out int pts_max_gap_ms, out int audio_underruns, \
out int audio_output_restarts, \
out int video_delay_ms, out int av_skew_ms, out int audio_hold_ms, \
out bool reconnecting)";

    #[test]
    fn field_names_are_unique() {
        let mut names: Vec<&str> = FIELDS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn declaration_matches_the_published_signature() {
        assert_eq!(proc_declaration(), DECLARATION);
    }

    #[test]
    fn every_field_is_written_and_typed() {
        // A snapshot where every field carries a distinct value, so a copy
        // that reads the wrong member shows up.
        let snap = StatsSnapshot {
            buffer_fill_ms: 1,
            current_speed: 1.05,
            total_audio_frames: 2,
            total_video_frames: 3,
            pts_max_gap_ms: 4,
            audio_underruns: 5,
            audio_output_restarts: 6,
            video_delay_ms: 7,
            av_skew_ms: 8,
            audio_hold_ms: 9,
            reconnecting: true,
        };

        let values = snap.values();
        assert_eq!(values.len(), FIELDS.len());

        // Types line up with the declaration ...
        for ((name, kind), value) in FIELDS.iter().zip(values.iter()) {
            let matches = matches!(
                (kind, value),
                (StatKind::Int, StatValue::Int(_))
                    | (StatKind::Float, StatValue::Float(_))
                    | (StatKind::Bool, StatValue::Bool(_))
            );
            assert!(matches, "{name} has the wrong value kind: {value:?}");
        }

        // ... every integer field carries its own distinct value ...
        let ints: Vec<i64> = values
            .iter()
            .filter_map(|v| match v {
                StatValue::Int(i) => Some(*i),
                _ => None,
            })
            .collect();
        let mut sorted = ints.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ints.len(), "a field was written twice");
        assert!(!ints.contains(&0), "a field was not written");

        // ... and the non-integer fields are what the snapshot holds.
        assert_eq!(snap.get("current_speed"), Some(StatValue::Float(1.05)));
        assert_eq!(snap.get("reconnecting"), Some(StatValue::Bool(true)));

        // Spot-check the by-name accessor against the same snapshot.
        assert_eq!(snap.get("buffer_fill_ms"), Some(StatValue::Int(1)));
        assert_eq!(snap.get("audio_hold_ms"), Some(StatValue::Int(9)));
        assert_eq!(snap.get("reconnect_count"), None);
    }

    #[test]
    fn a_default_snapshot_is_all_zero() {
        let values = StatsSnapshot::default().values();
        for (i, value) in values.iter().enumerate() {
            let zero = match value {
                StatValue::Int(v) => *v == 0,
                StatValue::Float(v) => *v == 0.0,
                StatValue::Bool(v) => !*v,
            };
            assert!(zero, "{} defaulted to {value:?}", FIELDS[i].0);
        }
    }
}
