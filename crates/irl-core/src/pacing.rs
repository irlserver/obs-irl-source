//! Video pacing queue (port of `receiver-video.c:82-209`): decoded frames in
//! system memory waiting for their due time, bounded by frame count and
//! bytes, with due times re-derived from the live audio playout offset.

use std::collections::VecDeque;

use crate::consts;

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
    entries: VecDeque<Entry<F>>,
    bytes: usize,
    peak: usize,
    overflows: u64,
    max_frames: usize,
    max_bytes: usize,
}

#[derive(Debug)]
struct Entry<F> {
    frame: F,
    pts_ns: i64,
    due_ns: u64,
    bytes: usize,
}

impl<F: PacedFrame> PacingQueue<F> {
    /// Empty queue with the given ceilings.
    pub fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            peak: 0,
            overflows: 0,
            max_frames,
            max_bytes,
        }
    }

    /// Whether another frame fits under both ceilings.
    pub fn has_room(&self) -> bool {
        self.entries.len() < self.max_frames && self.bytes < self.max_bytes
    }

    /// Append a frame with its current due time.
    pub fn push(&mut self, frame: F, due_ns: u64) {
        let entry = Entry {
            pts_ns: frame.pts_ns(),
            bytes: frame.bytes(),
            due_ns,
            frame,
        };
        self.bytes += entry.bytes;
        self.entries.push_back(entry);
        if self.entries.len() > self.peak {
            self.peak = self.entries.len();
        }
    }

    /// Re-derive every due time: `due = map(pts)`.
    ///
    /// Rescheduling against one offset per cycle preserves the spacing
    /// between frames (their due times differ only by their PTS deltas) and
    /// moves the whole queue with the audio it is mapped to, so video rides
    /// the same latency reclaim instead of trailing it for the depth of the
    /// queue.
    pub fn reschedule(&mut self, map: impl Fn(i64) -> u64) {
        for entry in &mut self.entries {
            entry.due_ns = map(entry.pts_ns);
        }
    }

    /// Head frame verdict at `now_ns`.
    ///
    /// Over the ceilings the head goes out early rather than being dropped:
    /// too-early video is what the un-paced path did all the time, and it
    /// beats a hole in the picture. As in C, a cycle spent over a ceiling
    /// counts an overflow even if the head happened to be due anyway.
    pub fn due_now(&mut self, now_ns: u64) -> Option<DueVerdict> {
        let due_ns = self.entries.front()?.due_ns;
        let over = !self.has_room();
        let delta = due_ns as i64 - now_ns as i64;

        if !over && delta > consts::VIDEO_PACING_SLACK_NS {
            return Some(DueVerdict::Wait(delta as u64));
        }
        if over {
            self.overflows += 1;
            return Some(DueVerdict::EmitEarly);
        }
        Some(DueVerdict::Emit)
    }

    /// Due time of the head frame, for the caller's sleep computation.
    pub fn next_due(&self) -> Option<u64> {
        self.entries.front().map(|e| e.due_ns)
    }

    /// Pop the head.
    pub fn pop(&mut self) -> Option<F> {
        let entry = self.entries.pop_front()?;
        self.bytes -= entry.bytes;
        Some(entry.frame)
    }

    /// Drain everything (clear / exit).
    pub fn drain(&mut self) -> Vec<F> {
        self.bytes = 0;
        self.entries.drain(..).map(|e| e.frame).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1080p NV12 frame's worth of bytes.
    const FRAME_BYTES: usize = 1920 * 1080 * 3 / 2;

    #[derive(Debug, PartialEq, Eq)]
    struct TestFrame {
        pts_ns: i64,
        bytes: usize,
    }

    impl PacedFrame for TestFrame {
        fn pts_ns(&self) -> i64 {
            self.pts_ns
        }
        fn bytes(&self) -> usize {
            self.bytes
        }
    }

    fn frame(pts_ns: i64) -> TestFrame {
        TestFrame {
            pts_ns,
            bytes: FRAME_BYTES,
        }
    }

    fn queue() -> PacingQueue<TestFrame> {
        PacingQueue::new(
            consts::VIDEO_PACING_MAX_FRAMES,
            consts::VIDEO_PACING_MAX_BYTES,
        )
    }

    #[test]
    fn frame_ceiling_bounds_the_queue() {
        let mut q = PacingQueue::new(consts::VIDEO_PACING_MAX_FRAMES, usize::MAX);
        for i in 0..consts::VIDEO_PACING_MAX_FRAMES {
            assert!(q.has_room(), "room at {i}");
            q.push(frame(i as i64), 0);
        }
        assert!(!q.has_room());
        assert_eq!(q.len(), consts::VIDEO_PACING_MAX_FRAMES);
    }

    #[test]
    fn byte_ceiling_bounds_the_queue() {
        let mut q = PacingQueue::new(usize::MAX, consts::VIDEO_PACING_MAX_BYTES);
        let mut pushed = 0;
        while q.has_room() {
            q.push(frame(pushed), 0);
            pushed += 1;
        }
        // 1 GiB of 1080p NV12 is ~345 frames, well inside the frame ceiling.
        assert!(q.bytes() >= consts::VIDEO_PACING_MAX_BYTES);
        assert_eq!(q.bytes(), pushed as usize * FRAME_BYTES);
        assert!(pushed < consts::VIDEO_PACING_MAX_FRAMES as i64);
    }

    #[test]
    fn bytes_track_push_and_pop() {
        let mut q = queue();
        q.push(frame(0), 0);
        q.push(frame(1), 0);
        assert_eq!(q.bytes(), 2 * FRAME_BYTES);
        q.pop();
        assert_eq!(q.bytes(), FRAME_BYTES);
        q.pop();
        assert_eq!(q.bytes(), 0);
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn reschedule_shifts_the_queue_and_preserves_spacing() {
        let mut q = queue();
        for i in 0..5 {
            q.push(frame(i * 16_000_000), 1_000 + i as u64 * 16_000_000);
        }

        // The offset the audio side publishes moves by +250 ms.
        q.reschedule(|pts| (pts + 250_000_000) as u64);
        let dues: Vec<u64> = q.entries.iter().map(|e| e.due_ns).collect();
        assert_eq!(dues[0], 250_000_000);
        for w in dues.windows(2) {
            assert_eq!(w[1] - w[0], 16_000_000, "spacing follows the PTS deltas");
        }

        // And back the other way: every due time is re-derived, not adjusted.
        q.reschedule(|pts| (pts + 100_000_000) as u64);
        assert_eq!(q.next_due(), Some(100_000_000));
    }

    #[test]
    fn empty_queue_has_no_verdict() {
        let mut q = queue();
        assert_eq!(q.due_now(1_000), None);
        assert_eq!(q.next_due(), None);
    }

    #[test]
    fn a_frame_in_the_future_waits() {
        let mut q = queue();
        q.push(frame(0), 100_000_000);
        assert_eq!(q.due_now(50_000_000), Some(DueVerdict::Wait(50_000_000)));
        assert_eq!(q.overflows(), 0);
    }

    #[test]
    fn slack_emits_rather_than_sleeping_again() {
        let mut q = queue();
        q.push(frame(0), 100_000_000);

        // Exactly one slack unit out: emit.
        let now = 100_000_000 - consts::VIDEO_PACING_SLACK_NS as u64;
        assert_eq!(q.due_now(now), Some(DueVerdict::Emit));
        // One nanosecond further out: sleep.
        assert_eq!(q.due_now(now - 1), Some(DueVerdict::Wait(1_000_001)));
    }

    #[test]
    fn a_due_or_late_frame_emits() {
        let mut q = queue();
        q.push(frame(0), 100_000_000);
        assert_eq!(q.due_now(100_000_000), Some(DueVerdict::Emit));
        assert_eq!(q.due_now(500_000_000), Some(DueVerdict::Emit));
        assert_eq!(q.overflows(), 0);
    }

    #[test]
    fn over_the_ceiling_emits_early_and_counts_an_overflow() {
        let mut q = PacingQueue::new(2, usize::MAX);
        q.push(frame(0), 1_000_000_000);
        q.push(frame(1), 1_016_000_000);
        assert!(!q.has_room());

        // Not remotely due, but a ceiling binds.
        assert_eq!(q.due_now(0), Some(DueVerdict::EmitEarly));
        assert_eq!(q.overflows(), 1);
        q.pop();

        // Back under the ceiling: normal pacing resumes.
        assert_eq!(q.due_now(0), Some(DueVerdict::Wait(1_016_000_000)));
        assert_eq!(q.overflows(), 1);
    }

    #[test]
    fn peak_is_a_high_water_mark() {
        let mut q = queue();
        for i in 0..7 {
            q.push(frame(i), 0);
        }
        assert_eq!(q.peak(), 7);
        for _ in 0..7 {
            q.pop();
        }
        assert_eq!(q.len(), 0);
        assert_eq!(q.peak(), 7, "peak survives the drain");
        q.push(frame(99), 0);
        assert_eq!(q.peak(), 7);
    }

    #[test]
    fn drain_returns_every_frame_in_order() {
        let mut q = queue();
        for i in 0..4 {
            q.push(frame(i * 1000), 0);
        }
        let frames = q.drain();
        assert_eq!(
            frames.iter().map(|f| f.pts_ns).collect::<Vec<_>>(),
            vec![0, 1000, 2000, 3000]
        );
        assert!(q.is_empty());
        assert_eq!(q.bytes(), 0);
        assert!(q.has_room());
        assert_eq!(q.drain().len(), 0);
    }

    #[test]
    fn frames_come_back_in_push_order() {
        let mut q = queue();
        q.push(frame(10), 1);
        q.push(frame(20), 2);
        assert_eq!(q.pop().unwrap().pts_ns, 10);
        assert_eq!(q.pop().unwrap().pts_ns, 20);
    }
}
