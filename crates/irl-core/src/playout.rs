//! The audio playout mapping: which stream PTS is playing at which OBS time.
//! Video is scheduled through it, so it is the lip-sync reference.

/// The latest chunk the audio pump handed to OBS, as the OBS time it ends at
/// and the stream PTS its content ends at, plus the baseline the offset
/// between the two is watched against.
///
/// The offset's absolute value carries the stream's PTS epoch and means
/// nothing on its own; only its movement does. Concealment advances the OBS
/// side while the stream side stands still, so an outage grows the offset by
/// its length, and that growth is what [`Self::drift_ns`] reports.
///
/// Two things move the offset on purpose, and the drift is net of both. The
/// audio hold grows it to wait for late video. The speed trim plays a sender
/// whose clock is not the OBS clock at that clock's rate, so the stream side
/// runs ahead of (or behind) the OBS side by the clock difference, forever:
/// 50 ppm is 3 ms a minute. Neither is latency the plugin added, and video
/// maps through the same offset, so neither moves lip sync.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutMapping {
    obs_end_ns: u64,
    pts_end_ns: i64,
    baseline: Option<Baseline>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Baseline {
    offset_ns: i64,
    /// The audio hold when the baseline was taken, so drift is measured net
    /// of how far it moved since.
    hold_ms: i32,
    /// How far the speed trim has moved the offset since the baseline.
    trim_moved_ns: i64,
}

impl PlayoutMapping {
    /// Record the chunk just handed to OBS: it ends at `obs_end_ns` on the OBS
    /// clock and at `pts_end_ns` in stream time.
    pub fn publish(&mut self, obs_end_ns: u64, pts_end_ns: i64) {
        self.obs_end_ns = obs_end_ns;
        self.pts_end_ns = pts_end_ns;
    }

    /// Whether a chunk has been handed to OBS since the mapping was last
    /// cleared, whether or not its stream PTS makes the mapping usable.
    pub fn has_output(&self) -> bool {
        self.obs_end_ns != 0
    }

    /// The stream PTS the latest chunk's content ends at.
    pub fn pts_end_ns(&self) -> i64 {
        self.pts_end_ns
    }

    /// Whether there is a mapping to schedule video through.
    pub fn is_published(&self) -> bool {
        self.obs_end_ns != 0 && self.pts_end_ns > 0
    }

    /// The stream PTS to OBS clock offset, once a mapping is published.
    pub fn offset_ns(&self) -> Option<i64> {
        self.is_published()
            .then(|| self.obs_end_ns as i64 - self.pts_end_ns)
    }

    /// The OBS time at which stream PTS `pts_ns` plays, once a mapping is
    /// published. A time before the OBS clock's epoch clamps to zero, which
    /// OBS takes as "now".
    pub fn map(&self, pts_ns: i64) -> Option<u64> {
        let mapped = pts_ns + self.offset_ns()?;
        Some(if mapped < 0 { 0 } else { mapped as u64 })
    }

    /// Take the current offset as the baseline drift is measured from, with
    /// `hold_ms` the audio hold in force. Only when a mapping is published and
    /// no baseline is held; returns whether one was taken.
    pub fn take_baseline(&mut self, hold_ms: i32) -> bool {
        if self.baseline.is_some() {
            return false;
        }
        let Some(offset_ns) = self.offset_ns() else {
            return false;
        };
        self.baseline = Some(Baseline {
            offset_ns,
            hold_ms,
            trim_moved_ns: 0,
        });
        true
    }

    /// Record a chunk of real audio that played `stream_ns` of stream time
    /// with the speed trim at `trim` (see `SpeedTrim::value`). Played at
    /// `1 + trim`, it takes `stream_ns / (1 + trim)` of OBS time, and the
    /// difference is the trim's part of the offset's movement. Ignored until
    /// a baseline is held, since the drift is only measured from one.
    pub fn note_trimmed_chunk(&mut self, stream_ns: u64, trim: f32) {
        let Some(baseline) = self.baseline.as_mut() else {
            return;
        };
        let trim = f64::from(trim);
        let stream_ns = stream_ns as f64;
        baseline.trim_moved_ns += (stream_ns / (1.0 + trim) - stream_ns).round() as i64;
    }

    /// How far the offset has grown past its baseline, net of what the audio
    /// hold (now `hold_ms`) and the speed trim moved it by since. `None`
    /// without a published mapping or a baseline.
    pub fn drift_ns(&self, hold_ms: i32) -> Option<i64> {
        let offset_ns = self.offset_ns()?;
        let baseline = self.baseline?;
        let hold_moved_ns = i64::from(hold_ms - baseline.hold_ms) * 1_000_000;
        Some(offset_ns - baseline.offset_ns - hold_moved_ns - baseline.trim_moved_ns)
    }

    /// Forget the latest chunk and the baseline.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published(obs_end_ns: u64, pts_end_ns: i64) -> PlayoutMapping {
        let mut mapping = PlayoutMapping::default();
        mapping.publish(obs_end_ns, pts_end_ns);
        mapping
    }

    #[test]
    fn mapping_shifts_the_pts_onto_the_obs_clock() {
        // Audio whose stream PTS ends at 10 s plays out at 3 s on the OBS
        // clock: everything is shifted back by 7 s.
        let mapping = published(3_000_000_000, 10_000_000_000);
        assert_eq!(mapping.map(10_000_000_000), Some(3_000_000_000));
        assert_eq!(mapping.map(10_016_000_000), Some(3_016_000_000));
        assert_eq!(mapping.offset_ns(), Some(-7_000_000_000));
    }

    #[test]
    fn negative_mapped_pts_clamps_to_zero() {
        // A frame from before the audio epoch would map before the OBS clock
        // started; OBS takes 0 as "now".
        assert_eq!(published(1_000_000_000, 10_000_000_000).map(0), Some(0));
        assert_eq!(published(1, 1).map(-5), Some(0));
    }

    #[test]
    fn nothing_maps_until_both_ends_are_usable() {
        assert!(!PlayoutMapping::default().has_output());
        assert_eq!(PlayoutMapping::default().map(0), None);
        // A chunk went out, but its content carried no usable stream PTS.
        let no_pts = published(5_000_000_000, 0);
        assert!(no_pts.has_output());
        assert!(!no_pts.is_published());
        assert_eq!(no_pts.offset_ns(), None);
        assert_eq!(no_pts.map(1_000), None);
        assert!(published(5_000_000_000, 1).is_published());
    }

    #[test]
    fn drift_is_measured_from_the_first_baseline() {
        let mut mapping = published(3_000_000_000, 10_000_000_000);
        assert_eq!(mapping.drift_ns(0), None, "no baseline yet");
        assert!(mapping.take_baseline(0));
        assert!(!mapping.take_baseline(0), "a held baseline stays");
        assert_eq!(mapping.drift_ns(0), Some(0));

        // A 300 ms outage concealed: the OBS side ran on, the stream side
        // stood still.
        mapping.publish(3_300_000_000, 10_000_000_000);
        assert_eq!(mapping.drift_ns(0), Some(300_000_000));
    }

    #[test]
    fn drift_is_net_of_the_hold_moving_the_offset() {
        let mut mapping = published(3_000_000_000, 10_000_000_000);
        assert!(mapping.take_baseline(100));
        // The hold rose by 200 ms and the offset followed it.
        mapping.publish(3_200_000_000, 10_000_000_000);
        assert_eq!(mapping.drift_ns(300), Some(0));
        // A hold released below the baseline's reads the other way.
        assert_eq!(mapping.drift_ns(0), Some(300_000_000));
    }

    #[test]
    fn drift_is_net_of_the_trim_following_the_sender_clock() {
        // A sender 50 ppm fast, played at that rate: every second of stream
        // time takes a little less than a second of OBS time, and after an
        // hour the stream side is 180 ms further ahead.
        let trim = 50e-6_f32;
        let chunk_ns = 1_000_000_000_u64;
        let mut mapping = published(3_000_000_000, 10_000_000_000);
        assert!(mapping.take_baseline(0));
        let (mut obs_ns, mut pts_ns) = (3_000_000_000_f64, 10_000_000_000_i64);
        for _ in 0..3600 {
            obs_ns += chunk_ns as f64 / (1.0 + f64::from(trim));
            pts_ns += chunk_ns as i64;
            mapping.publish(obs_ns.round() as u64, pts_ns);
            mapping.note_trimmed_chunk(chunk_ns, trim);
        }
        let moved_ns = mapping.offset_ns().unwrap() - (3_000_000_000 - 10_000_000_000);
        assert!(moved_ns < -170_000_000, "offset moved {moved_ns}ns");
        // Each chunk's share rounds to a whole nanosecond.
        let drift_ns = mapping.drift_ns(0).unwrap();
        assert!(drift_ns.abs() <= 3600 / 2, "drift {drift_ns}ns");

        // A 300 ms outage on top still reads as 300 ms.
        mapping.publish(obs_ns.round() as u64 + 300_000_000, pts_ns);
        let drift_ms = mapping.drift_ns(0).unwrap() as f64 / 1e6;
        assert!((drift_ms - 300.0).abs() < 0.01, "drift {drift_ms}ms");
    }

    #[test]
    fn the_trim_counts_only_from_the_baseline() {
        let mut mapping = published(3_000_000_000, 10_000_000_000);
        mapping.note_trimmed_chunk(1_000_000_000, 0.01);
        assert!(mapping.take_baseline(0));
        assert_eq!(mapping.drift_ns(0), Some(0));
    }

    #[test]
    fn no_baseline_is_taken_without_a_mapping() {
        let mut mapping = PlayoutMapping::default();
        assert!(!mapping.take_baseline(0));
        mapping.publish(5_000_000_000, 0);
        assert!(!mapping.take_baseline(0));
        assert_eq!(mapping.drift_ns(0), None);
    }

    #[test]
    fn clearing_forgets_the_baseline() {
        let mut mapping = published(3_000_000_000, 10_000_000_000);
        assert!(mapping.take_baseline(0));

        mapping.clear();
        assert_eq!(mapping, PlayoutMapping::default());
        mapping.publish(3_050_000_000, 10_000_000_000);
        assert_eq!(mapping.drift_ns(0), None);
    }
}
