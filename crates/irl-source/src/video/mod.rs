//! Video path. W2-C.
pub mod intake;
pub mod output;
pub mod thread;

use std::sync::Arc;

use crate::shared::Shared;

pub fn video_thread(shared: Arc<Shared>) {
    let _ = shared;
    todo!("W2-C")
}
