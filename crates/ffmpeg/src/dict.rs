//! `AVDictionary` for demuxer options.

use core::ffi::CStr;

use crate::Error;

/// An owned `AVDictionary*` (may be null when empty).
pub struct Dictionary(*mut ffmpeg_sys_next::AVDictionary);

impl Dictionary {
    pub fn new() -> Self {
        Self(core::ptr::null_mut())
    }

    /// `av_dict_set(&dict, key, value, 0)` — later sets overwrite earlier ones.
    pub fn set(&mut self, key: &CStr, value: &CStr) -> crate::Result<()> {
        // SAFETY: `&mut self.0` is a valid `AVDictionary**` (null means "allocate
        // one"); both strings are NUL-terminated and live for the call, and flag
        // 0 makes av_dict_set copy them.
        let ret =
            unsafe { ffmpeg_sys_next::av_dict_set(&mut self.0, key.as_ptr(), value.as_ptr(), 0) };
        Error::check(ret)
    }

    /// Consume the dictionary for `avformat_open_input` (which takes it by
    /// `&mut` and leaves the unconsumed entries behind).
    #[doc(hidden)]
    pub fn as_mut_ptr(&mut self) -> *mut *mut ffmpeg_sys_next::AVDictionary {
        &mut self.0
    }

    /// Number of entries left in the dictionary.
    pub fn len(&self) -> usize {
        self.iter_keys().count()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_null()
    }

    /// Keys left after `avformat_open_input`, i.e. options the demuxer did not
    /// recognise (logged by the C plugin).
    pub fn remaining_keys(&self) -> Vec<String> {
        self.iter_keys().collect()
    }

    fn iter_keys(&self) -> impl Iterator<Item = String> + '_ {
        let mut prev: *const ffmpeg_sys_next::AVDictionaryEntry = core::ptr::null();
        core::iter::from_fn(move || {
            // SAFETY: `self.0` is either null (iteration ends immediately) or a
            // dictionary we own; `prev` is null on the first call and otherwise
            // the entry the previous call returned, which stays valid because
            // nothing mutates the dictionary while `&self` is borrowed.
            let entry = unsafe { ffmpeg_sys_next::av_dict_iterate(self.0, prev) };
            if entry.is_null() {
                return None;
            }
            prev = entry;
            // SAFETY: a non-null entry has a NUL-terminated `key` owned by the
            // dictionary; the String copy ends the borrow immediately.
            let key = unsafe { CStr::from_ptr((*entry).key) };
            Some(key.to_string_lossy().into_owned())
        })
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Dictionary {
    fn drop(&mut self) {
        // SAFETY: `&mut self.0` points at our owned dictionary pointer;
        // av_dict_free tolerates null and nulls it out afterwards.
        unsafe { ffmpeg_sys_next::av_dict_free(&mut self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_read_back_keys() {
        let mut dict = Dictionary::new();
        assert!(dict.is_empty());
        assert!(dict.remaining_keys().is_empty());

        dict.set(c"probesize", c"1000000").unwrap();
        dict.set(c"fflags", c"+genpts").unwrap();
        dict.set(c"probesize", c"5000000").unwrap();

        let mut keys = dict.remaining_keys();
        keys.sort();
        assert_eq!(keys, vec!["fflags".to_string(), "probesize".to_string()]);
        assert_eq!(dict.len(), 2);
        assert!(!dict.is_empty());
    }
}
