//! `AVDictionary` for demuxer options.

use core::ffi::CStr;

/// An owned `AVDictionary*` (may be null when empty).
pub struct Dictionary(*mut ffmpeg_sys_next::AVDictionary);

impl Dictionary {
    pub fn new() -> Self {
        Self(core::ptr::null_mut())
    }

    /// `av_dict_set(&dict, key, value, 0)` — later sets overwrite earlier ones.
    pub fn set(&mut self, key: &CStr, value: &CStr) -> crate::Result<()> {
        let _ = (key, value);
        todo!("W1-B")
    }

    /// Consume the dictionary for `avformat_open_input` (which takes it by
    /// `&mut` and leaves the unconsumed entries behind).
    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut *mut ffmpeg_sys_next::AVDictionary {
        &mut self.0
    }

    /// Keys left after `avformat_open_input`, i.e. options the demuxer did not
    /// recognise (logged by the C plugin).
    pub fn remaining_keys(&self) -> Vec<String> {
        todo!("W1-B: av_dict_iterate")
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Dictionary {
    fn drop(&mut self) {
        todo!("W1-B: av_dict_free")
    }
}
