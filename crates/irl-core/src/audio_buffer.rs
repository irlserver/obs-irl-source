//! PTS-aware audio ring buffer (port of `src/audio-buffer.c`).
//!
//! Interleaved PCM in a byte ring, sized in milliseconds, with a parallel
//! queue of `(pts_ns, size, consumed)` chunks so every read can report the
//! stream PTS of its oldest byte. The C version carried its own mutex; here
//! the caller wraps it in one (lock order: the audio state lock first).

use crate::consts;

/// What the pump peeks before deciding what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferState {
    /// Stream PTS (ns) of the oldest queued byte.
    pub oldest_pts_ns: i64,
    /// Fill in milliseconds.
    pub fill_ms: i32,
    /// Chunks queued.
    pub chunk_count: usize,
}

/// One entry of the PTS chunk queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtsChunk {
    /// Stream PTS (ns) of the chunk's first byte.
    pub pts_ns: i64,
    /// Bytes of PCM in this chunk.
    pub size: usize,
    /// Bytes already read.
    pub consumed: usize,
}

const EMPTY_CHUNK: PtsChunk = PtsChunk {
    pts_ns: 0,
    size: 0,
    consumed: 0,
};

/// The ring buffer.
#[derive(Debug)]
pub struct AudioBuffer {
    data: Vec<u8>,
    head: usize,
    tail: usize,
    fill: usize,
    chunks: [PtsChunk; consts::AUDIO_PTS_MAX_CHUNKS],
    chunk_head: usize,
    chunk_tail: usize,
    chunk_count: usize,
    sample_rate: i32,
    channels: i32,
    bytes_per_sample: i32,
    target_ms: i32,
    min_ms: i32,
    max_ms: i32,
}

impl AudioBuffer {
    /// Allocate for `sample_rate`/`channels`/`bytes_per_sample` with the
    /// capacity `4 × max_ms` implies (plus headroom, as in C).
    ///
    /// Ports `audio_buffer_init`. The C function returns false only when the
    /// mutex could not be created; this type has no mutex (the caller wraps
    /// it), so the `Option` is never `None` — it keeps the shape of the C
    /// call site.
    pub fn new(
        sample_rate: i32,
        channels: i32,
        bytes_per_sample: i32,
        target_ms: i32,
        min_ms: i32,
        max_ms: i32,
    ) -> Option<Self> {
        let mut buf = Self {
            data: Vec::new(),
            head: 0,
            tail: 0,
            fill: 0,
            chunks: [EMPTY_CHUNK; consts::AUDIO_PTS_MAX_CHUNKS],
            chunk_head: 0,
            chunk_tail: 0,
            chunk_count: 0,
            sample_rate,
            channels,
            bytes_per_sample,
            target_ms,
            min_ms,
            max_ms,
        };
        let capacity = buf.capacity_for(max_ms);
        buf.data = vec![0u8; capacity];
        Some(buf)
    }

    /// Reinitialise for a new format (flushes).
    ///
    /// Ports `audio_buffer_reconfigure`. The watermarks are kept: the C
    /// function took them again only because the caller had them at hand,
    /// and always passed the values already in force.
    pub fn reconfigure(&mut self, sample_rate: i32, channels: i32, bytes_per_sample: i32) -> bool {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.bytes_per_sample = bytes_per_sample;
        let capacity = self.capacity_for(self.max_ms);
        self.data = vec![0u8; capacity];
        self.head = 0;
        self.tail = 0;
        self.fill = 0;
        self.chunk_head = 0;
        self.chunk_tail = 0;
        self.chunk_count = 0;
        true
    }

    /// Move the watermarks, growing the ring if needed while keeping every
    /// queued sample (linearising a wrapped ring). Never shrinks.
    pub fn resize(&mut self, target_ms: i32, min_ms: i32, max_ms: i32) -> bool {
        let wanted = self.capacity_for(max_ms);

        // Storage only ever grows. Shrinking would mean discarding queued
        // audio that does not fit, which is forbidden once playback has
        // primed; the ring is pure headroom above the watermarks, so leaving
        // it oversized until the next reconnect costs nothing but memory.
        if wanted > self.data.len() {
            // Linearise into the new allocation. Chunk metadata is relative
            // (size/consumed, no ring offsets), so the PTS queue survives the
            // move untouched.
            let mut grown = vec![0u8; wanted];
            let first = self.data.len() - self.tail;
            if first >= self.fill {
                grown[..self.fill].copy_from_slice(&self.data[self.tail..self.tail + self.fill]);
            } else {
                grown[..first].copy_from_slice(&self.data[self.tail..]);
                grown[first..self.fill].copy_from_slice(&self.data[..self.fill - first]);
            }
            self.data = grown;
            self.tail = 0;
            self.head = self.fill % self.data.len();
        }

        self.target_ms = target_ms;
        self.min_ms = min_ms;
        self.max_ms = max_ms;
        true
    }

    /// Drop everything.
    pub fn flush(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.fill = 0;
        self.chunk_head = 0;
        self.chunk_tail = 0;
        self.chunk_count = 0;
    }

    /// Append `samples` with the stream PTS of its first byte. An oversized
    /// write keeps the newest tail and advances the PTS accordingly; on
    /// overflow the oldest data is dropped. Returns bytes written.
    pub fn write_pts(&mut self, samples: &[u8], pts_ns: i64) -> usize {
        self.write_inner(samples, pts_ns)
    }

    /// Append without a PTS (continues from the previous chunk's end).
    pub fn write(&mut self, samples: &[u8]) -> usize {
        // Continuation marker: derive the new chunk's PTS from the prior
        // chunk's end so reads stay PTS-consistent. Without this, read_pts
        // would report 0 for these bytes while pts_consume drained unrelated
        // chunk metadata. Computed before the chunk ring is trimmed, as in C.
        let mut pts_ns = 0;
        if self.chunk_count > 0 && self.sample_rate > 0 && self.frame_size() > 0 {
            let last_idx =
                (self.chunk_head + consts::AUDIO_PTS_MAX_CHUNKS - 1) % consts::AUDIO_PTS_MAX_CHUNKS;
            let last = self.chunks[last_idx];
            let samples_in_chunk = (last.size / self.frame_size()) as i64;
            pts_ns = last.pts_ns + samples_in_chunk * 1_000_000_000 / self.sample_rate as i64;
        }
        self.write_inner(samples, pts_ns)
    }

    fn write_inner(&mut self, samples: &[u8], mut pts_ns: i64) -> usize {
        if self.data.is_empty() || samples.is_empty() {
            return 0;
        }

        while self.chunk_count >= consts::AUDIO_PTS_MAX_CHUNKS {
            self.skip_oldest_chunk();
        }

        let samples = self.clamp_incoming_to_capacity(samples, &mut pts_ns);
        if samples.is_empty() {
            return 0;
        }

        let written = self.ring_write(samples);
        if written > 0 {
            self.chunks[self.chunk_head] = PtsChunk {
                pts_ns,
                size: written,
                consumed: 0,
            };
            self.chunk_head = (self.chunk_head + 1) % consts::AUDIO_PTS_MAX_CHUNKS;
            self.chunk_count += 1;
        }
        written
    }

    /// Read up to `out.len()` bytes; returns bytes read and the stream PTS of
    /// the first byte (interpolated inside a partially consumed chunk).
    pub fn read_pts(&mut self, out: &mut [u8]) -> (usize, i64) {
        if self.data.is_empty() || out.is_empty() {
            return (0, 0);
        }

        // The PTS of the oldest data is taken before the read consumes it.
        let pts = self.oldest_pts();
        let got = self.ring_read(out);
        if got > 0 {
            self.pts_consume(got);
        }
        (got, pts)
    }

    /// Read up to `out.len()` bytes without reporting a PTS
    /// (`audio_buffer_read`).
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        if self.data.is_empty() || out.is_empty() {
            return 0;
        }
        let got = self.ring_read(out);
        if got > 0 {
            self.pts_consume(got);
        }
        got
    }

    /// Read applying a linear 1→0 fade across the whole read.
    ///
    /// Float sample format is assumed, as in C; the fade is skipped when the
    /// buffer holds anything else.
    pub fn read_with_fade_out(&mut self, out: &mut [u8]) -> usize {
        let got = self.read(out);
        if got == 0 || self.frame_size() == 0 {
            return got;
        }
        if self.bytes_per_sample != 4 || self.channels <= 0 {
            return got;
        }

        let total_frames = got / self.frame_size();
        if total_frames == 0 {
            return got;
        }
        let channels = self.channels as usize;
        for f in 0..total_frames {
            let gain = 1.0f32 - (f as f32) / (total_frames as f32);
            for ch in 0..channels {
                let at = (f * channels + ch) * 4;
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&out[at..at + 4]);
                let scaled = f32::from_le_bytes(bytes) * gain;
                out[at..at + 4].copy_from_slice(&scaled.to_le_bytes());
            }
        }
        got
    }

    /// Oldest PTS, fill and chunk count, or `None` when empty.
    pub fn peek_state(&self) -> Option<BufferState> {
        if self.data.is_empty() || self.chunk_count == 0 {
            return None;
        }
        Some(BufferState {
            oldest_pts_ns: self.oldest_pts(),
            fill_ms: self.fill_ms(),
            chunk_count: self.chunk_count,
        })
    }

    /// Discard the oldest chunk.
    pub fn skip_chunk(&mut self) {
        if self.data.is_empty() || self.chunk_count == 0 {
            return;
        }
        self.skip_oldest_chunk();
    }

    /// Discard whole chunks until the oldest PTS is ≥ `min_pts_ns`. Returns chunks skipped.
    pub fn skip_until_pts(&mut self, min_pts_ns: i64) -> usize {
        if self.data.is_empty() {
            return 0;
        }
        let mut skipped = 0;
        while self.chunk_count > 0 {
            if self.oldest_pts() >= min_pts_ns {
                break;
            }
            self.skip_oldest_chunk();
            skipped += 1;
        }
        skipped
    }

    /// Discard oldest chunks until at most `keep_ms` remain, keeping at least
    /// `min_chunks`. Returns chunks trimmed and the resulting state.
    pub fn trim_to_keep_ms(
        &mut self,
        keep_ms: i32,
        min_chunks: usize,
    ) -> (usize, Option<BufferState>) {
        if self.data.is_empty() {
            return (0, None);
        }
        let mut trimmed = 0;
        while self.chunk_count > min_chunks && self.fill_ms() > keep_ms {
            self.skip_oldest_chunk();
            trimmed += 1;
        }
        (trimmed, self.peek_state())
    }

    /// Fill in milliseconds.
    pub fn fill_ms(&self) -> i32 {
        if self.frame_size() == 0 || self.sample_rate <= 0 {
            return 0;
        }
        let samples = (self.fill / self.frame_size()) as i64;
        (samples * 1000 / self.sample_rate as i64) as i32
    }

    /// Fill in bytes.
    pub fn fill_bytes(&self) -> usize {
        self.fill
    }

    /// `ms` converted to bytes at the current format.
    pub fn ms_to_bytes(&self, ms: i32) -> usize {
        self.ms_to_bytes_i64(ms as i64)
    }

    /// Current format.
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate
    }
    /// Current format.
    pub fn channels(&self) -> i32 {
        self.channels
    }
    /// Current format.
    pub fn bytes_per_sample(&self) -> i32 {
        self.bytes_per_sample
    }
    /// `bytes_per_sample * channels`.
    pub fn frame_size(&self) -> usize {
        let size = self.bytes_per_sample as i64 * self.channels as i64;
        if size <= 0 { 0 } else { size as usize }
    }
    /// Watermarks.
    pub fn target_ms(&self) -> i32 {
        self.target_ms
    }
    /// Watermarks.
    pub fn min_ms(&self) -> i32 {
        self.min_ms
    }
    /// Watermarks.
    pub fn max_ms(&self) -> i32 {
        self.max_ms
    }
    /// Ring capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Chunks queued.
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// True when the buffer holds at least `min_ms` (`audio_buffer_ready`).
    pub fn ready(&self) -> bool {
        self.fill_ms() >= self.min_ms
    }

    /// PTS of the oldest queued byte (`audio_buffer_peek_pts`): the oldest
    /// chunk's PTS advanced by the part of it already consumed. 0 when empty.
    pub fn peek_pts(&self) -> i64 {
        self.oldest_pts()
    }

    // ── internals ──

    fn ms_to_bytes_i64(&self, ms: i64) -> usize {
        if self.frame_size() == 0 || self.sample_rate <= 0 || ms <= 0 {
            return 0;
        }
        ((ms * self.sample_rate as i64 / 1000) as usize) * self.frame_size()
    }

    /// Allocate enough headroom that Max Buffer is not an audible hard trim
    /// point (`audio_buffer_init`: `ms_to_bytes(max_ms * 4)`, 65536 on a
    /// degenerate format).
    fn capacity_for(&self, max_ms: i32) -> usize {
        let capacity = self.ms_to_bytes_i64(max_ms as i64 * consts::BUFFER_CAPACITY_MULTIPLIER);
        if capacity == 0 {
            consts::AUDIO_BUFFER_FALLBACK_CAPACITY
        } else {
            capacity
        }
    }

    fn oldest_pts(&self) -> i64 {
        if self.chunk_count == 0 {
            return 0;
        }
        let c = self.chunks[self.chunk_tail];
        if self.sample_rate > 0 && self.frame_size() > 0 {
            let consumed_samples = (c.consumed / self.frame_size()) as i64;
            c.pts_ns + consumed_samples * 1_000_000_000 / self.sample_rate as i64
        } else {
            c.pts_ns
        }
    }

    /// If a single decoded chunk is larger than the allocated storage, keep
    /// only the newest tail so buffered mode does not start several hundred
    /// milliseconds behind by construction.
    fn clamp_incoming_to_capacity<'a>(&self, samples: &'a [u8], pts_ns: &mut i64) -> &'a [u8] {
        let capacity = self.data.len();
        if capacity == 0 || samples.len() <= capacity {
            return samples;
        }
        let skip_bytes = samples.len() - capacity;
        if self.sample_rate > 0 && self.frame_size() > 0 {
            let skipped_frames = (skip_bytes / self.frame_size()) as i64;
            *pts_ns += skipped_frames * 1_000_000_000 / self.sample_rate as i64;
        }
        &samples[skip_bytes..]
    }

    fn ring_write(&mut self, samples: &[u8]) -> usize {
        let capacity = self.data.len();
        let avail = capacity - self.fill;
        let to_write = samples.len().min(avail);
        if to_write == 0 {
            return 0;
        }

        let first_chunk = capacity - self.head;
        if first_chunk >= to_write {
            self.data[self.head..self.head + to_write].copy_from_slice(&samples[..to_write]);
        } else {
            self.data[self.head..].copy_from_slice(&samples[..first_chunk]);
            self.data[..to_write - first_chunk].copy_from_slice(&samples[first_chunk..to_write]);
        }

        self.head = (self.head + to_write) % capacity;
        self.fill += to_write;
        to_write
    }

    fn ring_read(&mut self, out: &mut [u8]) -> usize {
        let capacity = self.data.len();
        let to_read = out.len().min(self.fill);
        if to_read == 0 {
            return 0;
        }

        let first_chunk = capacity - self.tail;
        if first_chunk >= to_read {
            out[..to_read].copy_from_slice(&self.data[self.tail..self.tail + to_read]);
        } else {
            out[..first_chunk].copy_from_slice(&self.data[self.tail..]);
            out[first_chunk..to_read].copy_from_slice(&self.data[..to_read - first_chunk]);
        }

        self.tail = (self.tail + to_read) % capacity;
        self.fill -= to_read;
        to_read
    }

    fn skip_oldest_chunk(&mut self) {
        if self.data.is_empty() || self.chunk_count == 0 {
            return;
        }
        let c = self.chunks[self.chunk_tail];
        let remaining = c.size - c.consumed;
        if remaining > 0 && remaining <= self.fill {
            self.tail = (self.tail + remaining) % self.data.len();
            self.fill -= remaining;
        }
        self.chunk_tail = (self.chunk_tail + 1) % consts::AUDIO_PTS_MAX_CHUNKS;
        self.chunk_count -= 1;
    }

    /// Retire PTS chunks as data is consumed.
    fn pts_consume(&mut self, bytes_consumed: usize) {
        let mut remaining = bytes_consumed;
        while remaining > 0 && self.chunk_count > 0 {
            let c = &mut self.chunks[self.chunk_tail];
            let avail = c.size - c.consumed;
            if remaining >= avail {
                remaining -= avail;
                self.chunk_tail = (self.chunk_tail + 1) % consts::AUDIO_PTS_MAX_CHUNKS;
                self.chunk_count -= 1;
            } else {
                c.consumed += remaining;
                remaining = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: i32 = 48_000;
    const CHANNELS: i32 = 2;
    const BPS: i32 = 4;
    /// 960 frames (20 ms at 48 kHz) of stereo float.
    const CHUNK_BYTES: usize = 960 * 2 * 4;

    fn buffer(target_ms: i32) -> AudioBuffer {
        AudioBuffer::new(
            RATE,
            CHANNELS,
            BPS,
            target_ms,
            target_ms / 2,
            target_ms + 200,
        )
        .unwrap()
    }

    /// A chunk whose every float sample equals `value`.
    fn pcm(value: f32, frames: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 2 * 4);
        for _ in 0..frames * 2 {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    fn floats(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    #[test]
    fn roundtrip_preserves_pts() {
        let mut buf = buffer(120);
        assert_eq!(buf.write_pts(&pcm(0.5, 960), 1_000_000_000), CHUNK_BYTES);
        assert_eq!(buf.write_pts(&pcm(0.25, 960), 1_020_000_000), CHUNK_BYTES);

        let mut out = vec![0u8; CHUNK_BYTES];
        let (got, pts) = buf.read_pts(&mut out);
        assert_eq!(got, CHUNK_BYTES);
        assert_eq!(pts, 1_000_000_000);
        assert!(floats(&out).iter().all(|&s| s == 0.5));

        let (got, pts) = buf.read_pts(&mut out);
        assert_eq!(got, CHUNK_BYTES);
        assert_eq!(pts, 1_020_000_000);
        assert!(floats(&out).iter().all(|&s| s == 0.25));

        assert_eq!(buf.fill_bytes(), 0);
        assert_eq!(buf.chunk_count(), 0);
        assert_eq!(buf.peek_state(), None);
    }

    #[test]
    fn partial_read_interpolates_chunk_pts() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 960), 5_000_000_000);

        // Half the chunk: 480 frames = 10 ms.
        let mut out = vec![0u8; CHUNK_BYTES / 2];
        let (got, pts) = buf.read_pts(&mut out);
        assert_eq!(got, CHUNK_BYTES / 2);
        assert_eq!(pts, 5_000_000_000);

        assert_eq!(buf.peek_pts(), 5_010_000_000);
        let state = buf.peek_state().unwrap();
        assert_eq!(state.oldest_pts_ns, 5_010_000_000);
        assert_eq!(state.chunk_count, 1);
        assert_eq!(state.fill_ms, 10);

        let (got, pts) = buf.read_pts(&mut out);
        assert_eq!(got, CHUNK_BYTES / 2);
        assert_eq!(pts, 5_010_000_000);
    }

    #[test]
    fn chunk_ring_wraps_dropping_oldest() {
        // 20 ms chunks, ring capacity 4 × max_ms: a 2000 ms target keeps the
        // byte ring far larger than 256 chunks so the chunk ring is what binds.
        let mut buf = buffer(2000);
        assert!(buf.capacity() >= consts::AUDIO_PTS_MAX_CHUNKS * CHUNK_BYTES);

        for i in 0..consts::AUDIO_PTS_MAX_CHUNKS {
            let pts = i as i64 * 20_000_000;
            assert_eq!(buf.write_pts(&pcm(0.1, 960), pts), CHUNK_BYTES);
        }
        assert_eq!(buf.chunk_count(), consts::AUDIO_PTS_MAX_CHUNKS);
        assert_eq!(buf.peek_pts(), 0);

        // One more write drops the oldest chunk, bytes and all.
        buf.write_pts(&pcm(0.2, 960), 256 * 20_000_000);
        assert_eq!(buf.chunk_count(), consts::AUDIO_PTS_MAX_CHUNKS);
        assert_eq!(buf.peek_pts(), 20_000_000);
        assert_eq!(buf.fill_bytes(), consts::AUDIO_PTS_MAX_CHUNKS * CHUNK_BYTES);

        // ... and the newest chunk is the one just written.
        let skipped = buf.skip_until_pts(256 * 20_000_000);
        assert_eq!(skipped, consts::AUDIO_PTS_MAX_CHUNKS - 1);
        assert_eq!(buf.chunk_count(), 1);
        assert_eq!(buf.peek_pts(), 256 * 20_000_000);
    }

    #[test]
    fn oversized_write_keeps_newest_tail() {
        let mut buf = buffer(20); // max 220 ms, capacity 880 ms
        let capacity = buf.capacity();
        let frames = capacity / 8 + 480; // 480 frames (10 ms) over capacity
        let mut data = pcm(0.0, frames);
        // Mark the last frame so we can prove the tail survived.
        let last = (frames - 1) * 2 * 4;
        data[last..last + 4].copy_from_slice(&7.0f32.to_le_bytes());
        data[last + 4..last + 8].copy_from_slice(&7.0f32.to_le_bytes());

        let written = buf.write_pts(&data, 1_000_000_000);
        assert_eq!(written, capacity);
        assert_eq!(buf.fill_bytes(), capacity);
        // The PTS advanced by the 10 ms that was dropped off the front.
        assert_eq!(buf.peek_pts(), 1_010_000_000);

        let mut out = vec![0u8; capacity];
        let (got, _) = buf.read_pts(&mut out);
        assert_eq!(got, capacity);
        assert_eq!(floats(&out).last().copied(), Some(7.0));
    }

    #[test]
    fn resize_grows_never_shrinks() {
        let mut buf = buffer(120);
        let small = buf.capacity();
        assert!(buf.resize(1000, 500, 1200));
        let large = buf.capacity();
        assert!(large > small);
        assert_eq!(buf.target_ms(), 1000);
        assert_eq!(buf.min_ms(), 500);
        assert_eq!(buf.max_ms(), 1200);

        assert!(buf.resize(120, 60, 320));
        assert_eq!(buf.capacity(), large, "resize must never shrink storage");
        assert_eq!(buf.target_ms(), 120);
    }

    #[test]
    fn resize_below_capacity_is_watermark_only() {
        let mut buf = buffer(120);
        let capacity = buf.capacity();
        buf.write_pts(&pcm(0.5, 960), 42);
        assert!(buf.resize(100, 50, 300));
        assert_eq!(buf.capacity(), capacity);
        assert_eq!(buf.fill_bytes(), CHUNK_BYTES);
        assert_eq!(buf.peek_pts(), 42);
        assert_eq!(buf.min_ms(), 50);
    }

    #[test]
    fn resize_linearises_wrapped_ring_keeping_every_sample() {
        let mut buf = buffer(20); // capacity 880 ms
        let capacity = buf.capacity();
        let chunk_frames = 960;

        // Fill, drain most of it, then write again so the ring wraps.
        let mut written_chunks = 0;
        while buf.fill_bytes() + CHUNK_BYTES <= capacity {
            buf.write_pts(
                &pcm(written_chunks as f32, chunk_frames),
                written_chunks * 20_000_000,
            );
            written_chunks += 1;
        }
        let mut sink = vec![0u8; CHUNK_BYTES * 3];
        let (drained, _) = buf.read_pts(&mut sink);
        assert_eq!(drained, CHUNK_BYTES * 3);
        for i in 0..3 {
            buf.write_pts(
                &pcm((written_chunks + i) as f32, chunk_frames),
                (written_chunks + i) * 20_000_000,
            );
        }
        let fill_before = buf.fill_bytes();
        let chunks_before = buf.chunk_count();
        let pts_before = buf.peek_pts();
        assert!(fill_before > 0);

        assert!(buf.resize(400, 200, 600));
        assert!(buf.capacity() > capacity);
        assert_eq!(buf.fill_bytes(), fill_before);
        assert_eq!(buf.chunk_count(), chunks_before);
        assert_eq!(buf.peek_pts(), pts_before);

        // Every chunk still reads back in order with its own marker value.
        let mut expected = 3i64;
        while buf.chunk_count() > 0 {
            let mut out = vec![0u8; CHUNK_BYTES];
            let (got, pts) = buf.read_pts(&mut out);
            assert_eq!(got, CHUNK_BYTES);
            assert_eq!(pts, expected * 20_000_000);
            assert!(floats(&out).iter().all(|&s| s == expected as f32));
            expected += 1;
        }
        assert_eq!(expected as usize, written_chunks as usize + 3);
    }

    #[test]
    fn write_without_pts_continues_from_previous_chunk_end() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 960), 3_000_000_000);
        // 960 frames at 48 kHz = 20 ms.
        assert_eq!(buf.write(&pcm(2.0, 480)), 480 * 8);

        let mut out = vec![0u8; CHUNK_BYTES];
        let (_, pts) = buf.read_pts(&mut out);
        assert_eq!(pts, 3_000_000_000);
        let (_, pts) = buf.read_pts(&mut out);
        assert_eq!(pts, 3_020_000_000);
    }

    #[test]
    fn trim_respects_min_chunks() {
        let mut buf = buffer(2000);
        for i in 0..10 {
            buf.write_pts(&pcm(0.1, 960), i * 20_000_000);
        }
        assert_eq!(buf.fill_ms(), 200);

        // keep_ms below one chunk, but min_chunks holds three back.
        let (trimmed, state) = buf.trim_to_keep_ms(0, 3);
        assert_eq!(trimmed, 7);
        let state = state.unwrap();
        assert_eq!(state.chunk_count, 3);
        assert_eq!(state.fill_ms, 60);
        assert_eq!(state.oldest_pts_ns, 7 * 20_000_000);

        // Nothing to do when already under the ceiling.
        let (trimmed, _) = buf.trim_to_keep_ms(1000, 1);
        assert_eq!(trimmed, 0);
    }

    #[test]
    fn skip_chunk_drops_one() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 960), 0);
        buf.write_pts(&pcm(1.0, 960), 20_000_000);
        buf.skip_chunk();
        assert_eq!(buf.chunk_count(), 1);
        assert_eq!(buf.fill_bytes(), CHUNK_BYTES);
        assert_eq!(buf.peek_pts(), 20_000_000);
    }

    #[test]
    fn fade_out_ramps_one_to_zero() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 4), 0);

        let mut out = vec![0u8; 4 * 2 * 4];
        let got = buf.read_with_fade_out(&mut out);
        assert_eq!(got, 4 * 2 * 4);

        let samples = floats(&out);
        // 4 frames, stereo: gains 1.0, 0.75, 0.5, 0.25 (linear 1 → 0).
        assert_eq!(samples, vec![1.0, 1.0, 0.75, 0.75, 0.5, 0.5, 0.25, 0.25]);
    }

    #[test]
    fn ms_to_bytes_and_fill_ms_match_the_format() {
        let buf = buffer(120);
        assert_eq!(buf.ms_to_bytes(20), CHUNK_BYTES);
        assert_eq!(buf.ms_to_bytes(0), 0);
        assert_eq!(buf.frame_size(), 8);
        assert_eq!(
            buf.capacity(),
            (320 * 4) as usize * RATE as usize / 1000 * 8,
            "capacity is 4 x max_ms"
        );
    }

    #[test]
    fn reconfigure_flushes_and_resizes() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 960), 1);
        assert!(buf.reconfigure(44_100, 1, 4));
        assert_eq!(buf.fill_bytes(), 0);
        assert_eq!(buf.chunk_count(), 0);
        assert_eq!(buf.sample_rate(), 44_100);
        assert_eq!(buf.channels(), 1);
        assert_eq!(buf.frame_size(), 4);
        assert_eq!(buf.capacity(), (320 * 4) * 44_100 / 1000 * 4);
    }

    #[test]
    fn flush_empties_the_ring() {
        let mut buf = buffer(120);
        buf.write_pts(&pcm(1.0, 960), 1);
        buf.flush();
        assert_eq!(buf.fill_bytes(), 0);
        assert_eq!(buf.chunk_count(), 0);
        assert!(!buf.ready());
    }
}
