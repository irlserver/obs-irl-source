//! `AVPacket`.

use crate::Error;

pub struct Packet(*mut ffmpeg_sys_next::AVPacket);

// SAFETY: an AVPacket is a plain owned allocation with no thread affinity; the
// receiver thread owns the one the plugin uses for its whole lifetime, and
// nothing else holds a pointer to it.
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
