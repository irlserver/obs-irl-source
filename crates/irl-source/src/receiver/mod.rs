//! Receiver and audio thread bodies (port of `src/receiver.c`). W2-A.

pub mod audio_in;
pub mod decode;
pub mod stream;

use std::sync::Arc;

use crate::shared::Shared;

pub fn receiver_thread(shared: Arc<Shared>) {
    let _ = shared;
    todo!("W2-A")
}

pub fn audio_thread(shared: Arc<Shared>) {
    let _ = shared;
    todo!("W2-B")
}
