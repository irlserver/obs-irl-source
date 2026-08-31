//! `AVPacket`.

use crate::Error;

pub struct Packet(*mut ffmpeg_sys_next::AVPacket);

// SAFETY: an AVPacket is a plain owned allocation with no thread affinity. The
// receiver thread owns the one it reads into for the whole run; the references
// it hands to the video thread through `new_ref` are separate allocations
// sharing a refcounted AVBufferRef, whose refcount is atomic.
unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> crate::Result<Self> {
        // SAFETY: no arguments; returns a fresh allocation or null.
        let ptr = unsafe { ffmpeg_sys_next::av_packet_alloc() };
        if ptr.is_null() {
            return Err(Error::nomem());
        }
        Ok(Self(ptr))
    }

    pub fn stream_index(&self) -> i32 {
        // SAFETY: `self.0` is a live packet we allocated and never null.
        unsafe { (*self.0).stream_index }
    }

    pub fn is_key(&self) -> bool {
        // SAFETY: as above.
        (unsafe { (*self.0).flags } & ffmpeg_sys_next::AV_PKT_FLAG_KEY) != 0
    }

    pub fn pts(&self) -> i64 {
        // SAFETY: as above.
        unsafe { (*self.0).pts }
    }

    pub fn dts(&self) -> i64 {
        // SAFETY: as above.
        unsafe { (*self.0).dts }
    }

    /// Presentation timestamp, falling back to the decode timestamp, `None`
    /// when neither is set. Mirrors [`crate::Frame::best_effort_pts`]: live
    /// demuxers leave one or both unset often enough that the caller must not
    /// see `AV_NOPTS_VALUE` as a number.
    pub fn pts_or_dts(&self) -> Option<i64> {
        // SAFETY: as above.
        let (pts, dts) = unsafe { ((*self.0).pts, (*self.0).dts) };
        [pts, dts]
            .into_iter()
            .find(|&v| v != ffmpeg_sys_next::AV_NOPTS_VALUE)
    }

    /// A new packet sharing this one's buffer (`av_packet_ref`).
    ///
    /// The receiver reads every packet into one reusable `Packet` and unrefs
    /// it immediately, so anything handed to another thread needs its own
    /// reference. The payload is not copied — both packets point at the same
    /// refcounted buffer, and it is freed when the last one drops.
    pub fn new_ref(&self) -> crate::Result<Self> {
        let dst = Self::new()?;
        // SAFETY: `dst` is a fresh blank packet and `self.0` is a live one;
        // av_packet_ref takes a reference to the source's buffer (or copies it
        // when the source is not refcounted) and leaves the source untouched.
        let ret = unsafe { ffmpeg_sys_next::av_packet_ref(dst.0, self.0) };
        if ret < 0 {
            return Err(Error(ret));
        }
        Ok(dst)
    }

    pub fn size(&self) -> i32 {
        // SAFETY: as above.
        unsafe { (*self.0).size }
    }

    /// `av_packet_unref`.
    pub fn unref(&mut self) {
        // SAFETY: as above; av_packet_unref resets the packet to the blank
        // state av_packet_alloc produced, which is valid to reuse.
        unsafe { ffmpeg_sys_next::av_packet_unref(self.0) };
    }

    #[doc(hidden)]
    pub fn as_ptr(&self) -> *const ffmpeg_sys_next::AVPacket {
        self.0
    }

    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg_sys_next::AVPacket {
        self.0
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: `&mut self.0` points at our sole owning pointer;
        // av_packet_free unrefs any data first and nulls the pointer.
        unsafe { ffmpeg_sys_next::av_packet_free(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_is_blank_and_unref_is_idempotent() {
        let mut pkt = Packet::new().unwrap();
        assert_eq!(pkt.size(), 0);
        assert_eq!(pkt.stream_index(), 0);
        assert!(!pkt.is_key());
        assert_eq!(pkt.pts(), ffmpeg_sys_next::AV_NOPTS_VALUE);
        pkt.unref();
        pkt.unref();
        assert_eq!(pkt.size(), 0);
    }
}
