//! Panic containment at the FFI boundary.
//!
//! A panic unwinding out of an `extern "C"` frame is undefined behavior (and
//! since Rust 1.81 an abort). Every shim in this crate therefore runs its body
//! through [`guard`], which converts a panic into one `LOG_ERROR` line and a
//! caller-supplied default.

use std::panic::{AssertUnwindSafe, UnwindSafe};

/// Run `f`, turning a panic into `default` after logging it as
/// `[<prefix>] panic in <what>: <message>`.
pub fn guard<R>(what: &'static str, default: R, f: impl FnOnce() -> R) -> R {
    let _ = (what, default, f);
    todo!("W1-A: catch_unwind(AssertUnwindSafe(f)), log_panic on Err")
}

/// [`guard`] for unit-returning callbacks.
pub fn guard_unit(what: &'static str, f: impl FnOnce()) {
    guard(what, (), f)
}

/// Extract a printable message from a panic payload (`&str` / `String`).
pub fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    let _ = payload;
    todo!("W1-A")
}

#[doc(hidden)]
pub fn _assert_unwind_safe<T>(t: T) -> AssertUnwindSafe<T> {
    AssertUnwindSafe(t)
}

#[doc(hidden)]
pub fn _is_unwind_safe<T: UnwindSafe>(_: &T) {}
