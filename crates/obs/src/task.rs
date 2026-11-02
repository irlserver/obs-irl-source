//! `obs_queue_task`: run work on a libobs task thread.

use core::ffi::c_void;

use crate::panic::guard_unit;

type UiTask = Box<dyn FnOnce() + Send>;

/// # Safety
/// `param` must be the `Box<UiTask>` [`queue_ui`] leaked, passed exactly once.
unsafe extern "C" fn ui_task_trampoline(param: *mut c_void) {
    guard_unit("ui task", || {
        // SAFETY: as documented above; libobs runs a queued task once.
        let task = unsafe { Box::from_raw(param.cast::<UiTask>()) };
        task();
    });
}

/// Run `task` on the UI thread, later. The thread to touch a source's live
/// settings from, because a properties dialog edits that same `obs_data_t` and
/// libobs does not lock it.
///
/// A task still queued when OBS shuts down never runs: the frontend drains its
/// event queue before `obs_shutdown` and the rest die with the application
/// object, so nothing calls back into an unloaded module. What `task` captured
/// is not dropped in that case, so it must not capture a source reference,
/// which would keep the source alive past its destroy.
pub fn queue_ui(task: UiTask) {
    let param = Box::into_raw(Box::new(task)).cast::<c_void>();
    // SAFETY: the trampoline matches `obs_task_t` and takes ownership of
    // `param`. Without `wait` this returns at once, so it cannot deadlock
    // against a UI thread joining the caller.
    unsafe {
        obs_sys::obs_queue_task(
            obs_sys::obs_task_type::OBS_TASK_UI,
            Some(ui_task_trampoline),
            param,
            false,
        );
    }
}
