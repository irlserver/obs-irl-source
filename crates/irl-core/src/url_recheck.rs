//! When to ask a provider again for the URL of a source that receives nothing.
//!
//! A key rotated on the provider's side leaves `url` holding a key the server
//! refuses, and the receiver would retry it forever. An offline sender looks
//! the same from here, so asking has to stay cheap for an outage of many hours
//! and has to stay quick for a rotated key, which rules out backing off while
//! the answer is unchanged.
//!
//! A changed answer does back off. The protocol asks providers for a stable
//! URL, and a provider that mints one per request would otherwise restart the
//! connection on every check. Backing off here bounds that without having to
//! tell the two kinds of provider apart.
//!
//! An outage is read from the decoded frame count, not from the media state.
//! The output clock stays primed across a reconnect, so the media state says
//! Playing during every open attempt after the first connection.

use crate::consts::{PROVIDER_RECHECK_FIRST_MS, PROVIDER_RECHECK_MAX_MS, PROVIDER_RECHECK_MS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outage {
    next_ms: u64,
    interval_ms: u64,
}

/// The watch's view of one source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UrlRecheck {
    frames: i64,
    outage: Option<Outage>,
}

impl UrlRecheck {
    /// One poll, with the frames decoded on the current connection so far.
    /// Whether to ask now. A `true` schedules the next check.
    pub fn poll(&mut self, now_ms: u64, frames: i64) -> bool {
        let arriving = frames > self.frames;
        self.frames = frames;
        if arriving {
            self.outage = None;
            return false;
        }
        let outage = self.outage.get_or_insert(Outage {
            next_ms: now_ms + PROVIDER_RECHECK_FIRST_MS,
            interval_ms: PROVIDER_RECHECK_MS,
        });
        if now_ms < outage.next_ms {
            return false;
        }
        outage.next_ms = now_ms + outage.interval_ms;
        true
    }

    /// The provider answered with a different URL.
    pub fn url_changed(&mut self) {
        if let Some(outage) = &mut self.outage {
            outage.interval_ms = (outage.interval_ms * 2).min(PROVIDER_RECHECK_MAX_MS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL_MS: usize = 5_000;

    /// The times the watch asks, polling every 5 s with a frame count that
    /// stays put.
    fn asks(recheck: &mut UrlRecheck, from_ms: u64, until_ms: u64, frames: i64) -> Vec<u64> {
        (from_ms..until_ms)
            .step_by(POLL_MS)
            .filter(|&now_ms| recheck.poll(now_ms, frames))
            .collect()
    }

    #[test]
    fn a_source_that_receives_frames_is_never_asked_about() {
        let mut recheck = UrlRecheck::default();
        for (i, now_ms) in (0..3_600_000).step_by(POLL_MS).enumerate() {
            assert!(!recheck.poll(now_ms, i as i64 + 1));
        }
    }

    #[test]
    fn a_short_outage_does_not_reach_the_provider() {
        let mut recheck = UrlRecheck::default();
        assert_eq!(
            asks(&mut recheck, 0, PROVIDER_RECHECK_FIRST_MS, 0),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn an_unchanged_answer_keeps_the_interval_for_a_long_outage() {
        let mut recheck = UrlRecheck::default();
        let seen = asks(&mut recheck, 0, 8 * 3_600_000, 0);
        assert_eq!(seen[0], PROVIDER_RECHECK_FIRST_MS);
        assert!(seen.windows(2).all(|w| w[1] - w[0] == PROVIDER_RECHECK_MS));
    }

    #[test]
    fn an_outage_after_streaming_counts_from_the_last_frame() {
        // The count stays at what the lost connection reached, then returns to
        // zero when the receiver tears that connection down.
        let mut recheck = UrlRecheck::default();
        assert!(!recheck.poll(0, 5000));
        assert!(!recheck.poll(5_000, 5000));
        assert!(!recheck.poll(10_000, 0));
        assert!(recheck.poll(5_000 + PROVIDER_RECHECK_FIRST_MS, 0));
    }

    #[test]
    fn frames_end_the_outage_and_the_next_one_starts_over() {
        let mut recheck = UrlRecheck::default();
        assert_eq!(asks(&mut recheck, 0, 40_000, 0), vec![30_000]);
        recheck.url_changed();
        assert!(!recheck.poll(45_000, 100));
        let later = 1_000_000;
        assert_eq!(
            asks(&mut recheck, later, later + PROVIDER_RECHECK_MS, 100),
            vec![later + PROVIDER_RECHECK_FIRST_MS]
        );
    }

    #[test]
    fn a_new_connection_with_frames_ends_the_outage_on_its_second_poll() {
        // Back from 5000 to 50 between two polls: not growth yet, so one poll
        // reads as an outage. The next one clears it, well inside the first wait.
        let mut recheck = UrlRecheck::default();
        assert!(!recheck.poll(0, 5000));
        assert!(!recheck.poll(5_000, 50));
        assert!(!recheck.poll(10_000, 200));
        assert!(!recheck.poll(10_000 + PROVIDER_RECHECK_FIRST_MS, 400));
    }

    #[test]
    fn a_url_that_keeps_changing_backs_off_to_the_ceiling() {
        let mut recheck = UrlRecheck::default();
        let mut seen = Vec::new();
        for now_ms in (0..12 * 3_600_000).step_by(POLL_MS) {
            if recheck.poll(now_ms, 0) {
                seen.push(now_ms);
                recheck.url_changed();
            }
        }
        let gaps: Vec<u64> = seen.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(gaps[0], PROVIDER_RECHECK_MS);
        assert_eq!(gaps[1], 2 * PROVIDER_RECHECK_MS);
        assert_eq!(gaps[2], 4 * PROVIDER_RECHECK_MS);
        assert_eq!(*gaps.last().unwrap(), PROVIDER_RECHECK_MAX_MS);
        assert!(gaps.iter().all(|&gap| gap <= PROVIDER_RECHECK_MAX_MS));
    }
}
