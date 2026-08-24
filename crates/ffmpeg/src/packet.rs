//! `AVPacket`.

pub struct Packet(*mut ffmpeg_sys_next::AVPacket);

unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> crate::Result<Self> {
        todo!("W1-B: av_packet_alloc")
    }

    pub fn stream_index(&self) -> i32 {
        todo!("W1-B")
    }

    pub fn is_key(&self) -> bool {
        todo!("W1-B: AV_PKT_FLAG_KEY")
    }

    pub fn pts(&self) -> i64 {
        todo!("W1-B")
    }

    pub fn size(&self) -> i32 {
        todo!("W1-B")
    }

    /// `av_packet_unref`.
    pub fn unref(&mut self) {
        todo!("W1-B")
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
        todo!("W1-B: av_packet_free")
    }
}
