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
    pub fn new(sample_rate: i32, channels: i32, bytes_per_sample: i32, target_ms: i32, min_ms: i32, max_ms: i32) -> Option<Self> {
        let _ = (sample_rate, channels, bytes_per_sample, target_ms, min_ms, max_ms);
        todo!("W1-C")
    }

    /// Reinitialise for a new format (flushes).
    pub fn reconfigure(&mut self, sample_rate: i32, channels: i32, bytes_per_sample: i32) -> bool {
        let _ = (sample_rate, channels, bytes_per_sample);
        todo!("W1-C")
    }

    /// Move the watermarks, growing the ring if needed while keeping every
    /// queued sample (linearising a wrapped ring). Never shrinks.
    pub fn resize(&mut self, target_ms: i32, min_ms: i32, max_ms: i32) -> bool {
        let _ = (target_ms, min_ms, max_ms);
        todo!("W1-C")
    }

    /// Drop everything.
    pub fn flush(&mut self) {
        todo!("W1-C")
    }

    /// Append `samples` with the stream PTS of its first byte. An oversized
    /// write keeps the newest tail and advances the PTS accordingly; on
    /// overflow the oldest data is dropped. Returns bytes written.
    pub fn write_pts(&mut self, samples: &[u8], pts_ns: i64) -> usize {
        let _ = (samples, pts_ns);
        todo!("W1-C")
    }

    /// Append without a PTS (continues from the previous chunk's end).
    pub fn write(&mut self, samples: &[u8]) -> usize {
        let _ = samples;
        todo!("W1-C")
    }

    /// Read up to `out.len()` bytes; returns bytes read and the stream PTS of
    /// the first byte (interpolated inside a partially consumed chunk).
    pub fn read_pts(&mut self, out: &mut [u8]) -> (usize, i64) {
        let _ = out;
        todo!("W1-C")
    }

    /// Read applying a linear 1→0 fade across the whole read.
    pub fn read_with_fade_out(&mut self, out: &mut [u8]) -> usize {
        let _ = out;
        todo!("W1-C")
    }

    /// Oldest PTS, fill and chunk count, or `None` when empty.
    pub fn peek_state(&self) -> Option<BufferState> {
        todo!("W1-C")
    }

    /// Discard the oldest chunk.
    pub fn skip_chunk(&mut self) {
        todo!("W1-C")
    }

    /// Discard whole chunks until the oldest PTS is ≥ `min_pts_ns`. Returns chunks skipped.
    pub fn skip_until_pts(&mut self, min_pts_ns: i64) -> usize {
        let _ = min_pts_ns;
        todo!("W1-C")
    }

    /// Discard oldest chunks until at most `keep_ms` remain, keeping at least
    /// `min_chunks`. Returns chunks trimmed and the resulting state.
    pub fn trim_to_keep_ms(&mut self, keep_ms: i32, min_chunks: usize) -> (usize, Option<BufferState>) {
        let _ = (keep_ms, min_chunks);
        todo!("W1-C")
    }

    /// Fill in milliseconds.
    pub fn fill_ms(&self) -> i32 {
        todo!("W1-C")
    }

    /// Fill in bytes.
    pub fn fill_bytes(&self) -> usize {
        self.fill
    }

    /// `ms` converted to bytes at the current format.
    pub fn ms_to_bytes(&self, ms: i32) -> usize {
        let _ = ms;
        todo!("W1-C")
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
        (self.bytes_per_sample * self.channels) as usize
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
        let _ = (self.head, self.tail, self.chunk_head, self.chunk_tail, self.chunk_count, &self.chunks);
        self.data.len()
    }
}
