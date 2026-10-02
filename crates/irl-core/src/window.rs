//! The extreme of a value over a sliding time window, shared by the audio
//! hold and the video delay.

use std::collections::VecDeque;

/// Which end of the values a [`WindowExtreme`] keeps.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Extreme {
    Lowest,
    Highest,
}

/// The lowest or highest value pushed within the last `window_ns`, kept as a
/// monotonic queue: a value that a newer, more extreme one has beaten can
/// never be the answer again, so it is dropped on the spot and every push is
/// amortised O(1).
#[derive(Debug)]
pub(crate) struct WindowExtreme {
    window_ns: u64,
    extreme: Extreme,
    /// `(when, value)`, oldest first, strictly less extreme from front to back.
    entries: VecDeque<(u64, i64)>,
}

impl WindowExtreme {
    pub(crate) fn new(window_ns: u64, extreme: Extreme) -> Self {
        Self {
            window_ns,
            extreme,
            entries: VecDeque::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn push(&mut self, now_ns: u64, value: i64) {
        while let Some(&(_, last)) = self.entries.back() {
            let beaten = match self.extreme {
                Extreme::Lowest => last >= value,
                Extreme::Highest => last <= value,
            };
            if !beaten {
                break;
            }
            self.entries.pop_back();
        }
        self.entries.push_back((now_ns, value));
        let horizon_ns = now_ns.saturating_sub(self.window_ns);
        while self.entries.front().is_some_and(|&(at, _)| at < horizon_ns) {
            self.entries.pop_front();
        }
    }

    pub(crate) fn value(&self) -> Option<i64> {
        self.entries.front().map(|&(_, value)| value)
    }
}
