//! Video pacing queue (port of `receiver-video.c:82-209`): decoded frames in
//! system memory waiting for their due time, bounded by frame count and
//! bytes, with due times re-derived from the live audio playout offset.

/// What the queue needs to know about a frame.
pub trait PacedFrame {
    /// Stream PTS in nanoseconds.
    fn pts_ns(&self) -> i64;
    /// Bytes held.
    fn bytes(&self) -> usize;
}

/// Decision for the head frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueVerdict {
    /// Emit now (due, or within slack).
    Emit,
    /// Emit now although not due: a ceiling is binding (counted as overflow).
    EmitEarly,
    /// Sleep this many nanoseconds (capped by the caller).
    Wait(u64),
}

/// The queue.
#[derive(Debug)]
pub struct PacingQueue<F> {
    entries: std::collections::VecDeque<Entry<F>>,
    bytes: usize,
    peak: usize,
    overflows: u64,
    max_frames: usize,
    max_bytes: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Entry<F> {
    frame: F,
    pts_ns: i64,
    due_ns: u64,
    bytes: usize,
}

impl<F: PacedFrame> PacingQueue<F> {
    /// Empty queue with the given ceilings.
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        let _ = (max_frames, max_bytes);
        todo!("W1-C")
    }

    /// Whether another frame fits under both ceilings.
    pub fn has_room(&self) -> bool {
        let _ = (&self.entries, self.bytes, self.max_frames, self.max_bytes);
        todo!("W1-C")
    }

    /// Append a frame with its current due time.
    pub fn push(&mut self, frame: F, due_ns: u64) {
        let _ = (frame, due_ns, self.peak);
        todo!("W1-C")
    }

    /// Re-derive every due time: `due = map(pts)`.
    pub fn reschedule(&mut self, map: impl Fn(i64) -> u64) {
        let _ = map;
        todo!("W1-C")
    }

    /// Head frame verdict at `now_ns`.
    pub fn due_now(&mut self, now_ns: u64) -> Option<DueVerdict> {
        let _ = (now_ns, self.overflows);
        todo!("W1-C")
    }

    /// Pop the head.
    pub fn pop(&mut self) -> Option<F> {
        todo!("W1-C")
    }

    /// Drain everything (clear / exit).
    pub fn drain(&mut self) -> Vec<F> {
        todo!("W1-C")
    }

    /// Frames queued.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes queued.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// High-water mark of frames queued.
    pub fn peak(&self) -> usize {
        self.peak
    }

    /// Frames emitted early because a ceiling bound.
    pub fn overflows(&self) -> u64 {
        self.overflows
    }
}
