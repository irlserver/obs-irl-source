//! Panic containment at the FFI boundary.
//!
//! A panic unwinding out of an `extern "C"` frame is undefined behavior (and
//! since Rust 1.81 an abort). Every shim in this crate therefore runs its body
//! through [`guard`], which converts a panic into one `LOG_ERROR` line and a
//! caller-supplied default.

use std::panic::{AssertUnwindSafe, UnwindSafe, catch_unwind};

use crate::log::{Level, blog_prefixed};

/// Run `f`, turning a panic into `default` after logging it as
/// `[<prefix>] panic in <what>: <message>`.
pub fn guard<R>(what: &'static str, default: R, f: impl FnOnce() -> R) -> R {
    // The closure is asserted unwind-safe rather than bounded on UnwindSafe:
    // every caller is an FFI shim whose state libobs owns, and there is no
    // "logically unobservable" invariant Rust could check for it. A panic here
    // is already a bug being contained, not a recoverable path.
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => {
            // Deliberately not `?`-propagated and never re-raised: the whole
            // point is that libobs sees a normal return.
            let message = payload_message(payload.as_ref());
            blog_prefixed(Level::Error, "obs", &format!("panic in {what}: {message}"));
            default
        }
    }
}

/// [`guard`] for unit-returning callbacks.
pub fn guard_unit(what: &'static str, f: impl FnOnce()) {
    guard(what, (), f)
}

/// Extract a printable message from a panic payload (`&str` / `String`).
pub fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[doc(hidden)]
pub fn _assert_unwind_safe<T>(t: T) -> AssertUnwindSafe<T> {
    AssertUnwindSafe(t)
}

#[doc(hidden)]
pub fn _is_unwind_safe<T: UnwindSafe>(_: &T) {}
