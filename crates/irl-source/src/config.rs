//! Settings loading, restart diffing and hot apply (port of
//! `config_load` / `config_requires_restart` / `config_apply_hot`). W2-D.

use obs::Data;

use crate::shared::{HotValues, StreamConfig};

/// The authoritative configuration held by the OBS thread.
#[derive(Debug, Clone)]
pub struct Config {
    pub stream: StreamConfig,
    pub hot: HotValues,
    pub close_when_inactive: bool,
}

impl Config {
    pub fn load(settings: &Data<'_>) -> Self {
        let _ = settings;
        todo!("W2-D")
    }

    /// URL, FFmpeg options, hardware decode and low-latency audio are latched
    /// at stream open.
    pub fn requires_restart(&self, other: &Self) -> bool {
        let _ = other;
        todo!("W2-D")
    }
}
