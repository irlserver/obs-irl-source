//! Audio hold for a sender whose video trails its audio.
//!
//! A phone whose video pipeline runs a stabiliser sends each video frame a
//! second or more after the audio of the same instant, both stamped with the
//! capture time (#33). At the receiver that shows as video with PTS `P`
//! arriving long after audio with PTS `P`: the audio has played by the time
//! the picture is in hand, and nothing done to the video schedule can put
//! the two back together, because a frame cannot be shown before it arrives.
//! The only correct response is what the media source does by pacing both
//! streams on their PTS: hold the audio back by the skew.
//!
//! The plugin does that by raising the jitter buffer's target for the
//! connection, before playback primes, by the amount audio has to wait so
//! that video with the same PTS is in hand a margin before it is due. The
//! speed controller then regulates around the raised target, the audio to
//! video mapping places video with an ordinary margin, and the standing
//! video delay ([`crate::video_delay`]) has nothing to cover. Audio starts
//! later by the hold, together with the picture, and nothing is inserted or
//! skipped to get there.
//!
//! The skew is measured in mux order: each video packet's PTS against the
//! newest audio PTS decoded before it. Mux order is the order the sender
//! wrote the two streams, so a probe burst or a batching relay does not
//! distort it the way wall-clock arrival would. The measurement is only
//! ever taken before priming, and the hold only ever rises within a
//! connection: raising the target once audio is live would build the extra
//! cushion at the -2% build rate over a minute or more, and the video delay
//! that covers the interim can never be taken back, so the two would end up
//! double-counting. A skew that appears mid-connection is left to the video
//! delay, as before.

/// How much longer audio must be held so that video stamped `skew_ms` behind
/// it is in hand `margin_ms` before it is due: the part of the skew that the
/// target and the output lead do not already cover, capped at `max_ms` and
/// never negative.
///
/// `skew_ms` is audio PTS minus video PTS at the same point in the mux, so a
/// positive value is video trailing audio. Audio with PTS `P` plays about
/// `target + lead` after it arrives; video with PTS `P` arrives `skew` after
/// that audio did, so its margin is `target + lead - skew`, and the hold is
/// whatever brings that up to `margin_ms`.
pub fn hold_ms(skew_ms: i64, target_ms: i32, lead_ms: i32, margin_ms: i32, max_ms: i32) -> i32 {
    let need = skew_ms + i64::from(margin_ms) - i64::from(target_ms) - i64::from(lead_ms);
    need.clamp(0, i64::from(max_ms)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: i32 = 120;
    const LEAD: i32 = 80;
    const MARGIN: i32 = 100;
    const MAX: i32 = 5000;

    #[test]
    fn a_sender_within_the_target_needs_no_hold() {
        // Target plus lead already covers 200 ms of skew, and the margin
        // takes 100 of that back.
        assert_eq!(hold_ms(0, TARGET, LEAD, MARGIN, MAX), 0);
        assert_eq!(hold_ms(100, TARGET, LEAD, MARGIN, MAX), 0);
        // Video ahead of audio is not a hold either.
        assert_eq!(hold_ms(-500, TARGET, LEAD, MARGIN, MAX), 0);
    }

    #[test]
    fn the_hold_is_the_uncovered_part_of_the_skew() {
        assert_eq!(hold_ms(101, TARGET, LEAD, MARGIN, MAX), 1);
        // The stream from #33: video 1.65 s behind its audio.
        assert_eq!(hold_ms(1650, TARGET, LEAD, MARGIN, MAX), 1550);
        // A deeper target covers more of it.
        assert_eq!(hold_ms(1650, 1000, LEAD, MARGIN, MAX), 670);
        // And one deeper than the skew covers all of it.
        assert_eq!(hold_ms(1650, 2000, LEAD, MARGIN, MAX), 0);
    }

    #[test]
    fn the_hold_is_capped() {
        assert_eq!(hold_ms(9_000, TARGET, LEAD, MARGIN, MAX), MAX);
        assert_eq!(hold_ms(i64::MAX / 2, TARGET, LEAD, MARGIN, MAX), MAX);
    }
}
