//! Three-tier PTS discontinuity repair (port of `src/pts-repair.c`).

/// What the caller should do with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtsAction {
    /// Use the PTS as is.
    Pass,
    /// PTS was repaired by interpolation.
    Interpolate,
    /// Insert `silence_ms` of silence before the frame.
    Silence,
    /// Large gap: full timeline reset.
    Reset,
}

/// Result of one evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// Action.
    pub action: PtsAction,
    /// The PTS to use (may differ from the input).
    pub corrected_pts: i64,
    /// Silence to insert (only for `Silence`).
    pub silence_ms: i32,
    /// Gap observed, in milliseconds (signed).
    pub gap_ms: i32,
}

/// Per-stream repair state.
#[derive(Debug, Clone)]
pub struct PtsRepair {
    last_pts: i64,
    last_duration: i64,
    tb_num: i32,
    tb_den: i32,
    small_gap_ms: i32,
    large_gap_ms: i32,
    last_gap_ms: i32,
    last_action_gap_ms: i32,
    consecutive_small_repairs: i32,
    relocking: bool,
    initialised: bool,
}

impl PtsRepair {
    /// `pts_repair_init`.
    pub fn new(small_gap_ms: i32, large_gap_ms: i32, tb_num: i32, tb_den: i32) -> Self {
        let _ = (small_gap_ms, large_gap_ms, tb_num, tb_den);
        todo!("W1-C")
    }

    /// `pts_repair_reset`.
    pub fn reset(&mut self) {
        let _ = (self.last_pts, self.last_duration, self.tb_num, self.tb_den, self.small_gap_ms, self.large_gap_ms,
                 self.last_gap_ms, self.last_action_gap_ms, self.consecutive_small_repairs, self.relocking, self.initialised);
        todo!("W1-C")
    }

    /// `pts_repair_evaluate`.
    pub fn evaluate(&mut self, pts: i64, duration: i64) -> Verdict {
        let _ = (pts, duration);
        todo!("W1-C")
    }

    /// Last known-good PTS and duration (stream time base), for extrapolating
    /// a frame that carries no timestamp.
    pub fn last(&self) -> Option<(i64, i64)> {
        todo!("W1-C")
    }
}
