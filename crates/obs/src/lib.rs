//! Safe wrapper over `obs-sys` for authoring OBS *sources* from Rust.
//!
//! This crate is the unsafe boundary for everything libobs: every raw pointer,
//! every `extern "C"` shim and every `catch_unwind` lives here. It knows
//! nothing about IRL streaming, but it is not a general libobs binding either:
//! it carries exactly what this plugin's async-video + audio input source uses
//! (registration, settings, properties, output, proc handlers, scene
//! transforms, the obs-websocket vendor API), and an API the plugin stops
//! calling is removed rather than kept for a hypothetical other caller.
//!
//! Conventions:
//! - Every `extern "C"` function exported or handed to libobs is wrapped in
//!   [`panic::guard`], so a Rust panic never unwinds into libobs.
//! - Timestamps that touch OBS come from [`time::gettime_ns`], never from
//!   `std::time::Instant`.
//! - Borrowed handles (`Data<'_>`, `SourceHandle`) are non-owning; `Owned*`
//!   types release in `Drop`.

pub mod audio;
pub mod data;
pub mod log;
pub mod module;
pub mod panic;
pub mod proc;
pub mod properties;
pub mod scene;
pub mod source;
pub mod time;
pub mod video;
pub mod websocket;

pub use audio::{AudioFormat, AudioFrame, SpeakerLayout};
pub use data::{Data, DataArray, OwnedData};
pub use proc::{CallData, ProcCallback, ProcHandler};
pub use properties::{
    ClickAction, IntList, IntProperty, ModifiedAction, Properties, PropertiesRef, Property,
    StringList, TextType,
};
pub use scene::{BoundsType, Scene, SceneItem, TransformInfo, VideoInfo};
pub use source::{MediaState, OwnedSource, Source, SourceHandle, register_source};
pub use video::{ColorRange, ColorSpace, VideoFormat, VideoFrame};

/// Re-exported so plugins can name raw types in the rare place they need to
/// (e.g. `OUTPUT_FLAGS` constants) without depending on `obs-sys` directly.
pub use obs_sys as sys;
