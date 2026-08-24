//! Source lifecycle (port of `src/irl-source.c`): create, destroy, update,
//! tick, the show/activate gating, the media controls and the stats proc.

use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::JoinHandle;

use irl_core::stats::{FIELDS, StatValue, StatsSnapshot, proc_declaration};
use parking_lot::Mutex;

use obs::scene::{BoundsType, Scene, TransformInfo};
use obs::{CallData, Data, MediaState, ProcCallback, Properties, Source, SourceHandle};

use crate::shared::{LifetimeStats, Shared, spawn_worker};
use crate::{audio, config::Config, receiver, video};

/// One run of the receiver: the state the three workers share plus their
/// handles. Dropping it is not enough to stop them — see [`stop_receiver`].
struct Running {
    shared: Arc<Shared>,
    receiver: JoinHandle<()>,
    audio: JoinHandle<()>,
    video: JoinHandle<()>,
}

/// Everything only the OBS thread touches. Behind one uncontended mutex
/// because the stats proc (obs-websocket's thread, a script's thread) reads
/// the running state too.
struct ObsState {
    /// Authoritative settings, kept for diffing the next `update`.
    config: Config,
    /// One-shot fit-to-canvas, armed for a source created without a URL.
    fit_pending: bool,
    /// The user pressed Stop in the media controls; survives show/activate.
    media_stopped: bool,
    running: Option<Running>,
}

/// The IRL source instance.
pub struct IrlSource {
    source: SourceHandle,
    /// Counters that outlive a connection (the set the C `reset_runtime_state`
    /// deliberately skipped).
    lifetime: Arc<LifetimeStats>,
    /// Owns the `get_stats` closure; dropped in [`Drop`], before libobs tears
    /// the source's proc handler down.
    proc_cb: Option<ProcCallback>,
    obs_state: Arc<Mutex<ObsState>>,
}

impl Source for IrlSource {
    const ID: &'static CStr = c"irl_source";
    const OUTPUT_FLAGS: u32 = obs::sys::OBS_SOURCE_ASYNC_VIDEO
        | obs::sys::OBS_SOURCE_AUDIO
        | obs::sys::OBS_SOURCE_DO_NOT_DUPLICATE
        | obs::sys::OBS_SOURCE_CONTROLLABLE_MEDIA;

    fn type_name() -> &'static CStr {
        crate::module_text(c"SourceName")
    }

    fn defaults(settings: &Data<'_>) {
        crate::settings::defaults(settings)
    }

    fn properties(instance: Option<&Self>) -> Properties {
        crate::settings::properties(instance)
    }

    fn create(settings: &Data<'_>, source: SourceHandle) -> Option<Box<Self>> {
        let config = Config::load(settings);
        apply_async_audio_mode(source, &config);

        let lifetime = Arc::new(LifetimeStats::default());
        // A source the user just added has no URL yet; one restored from a
        // scene collection always does. See `fit_to_canvas`.
        let fit_pending = config.url().is_none();
        let obs_state = Arc::new(Mutex::new(ObsState {
            config,
            fit_pending,
            media_stopped: false,
            running: None,
        }));

        // Register the stats proc so scripts and overlays can query state.
        // The obs-websocket vendor extension calls this same proc, so both
        // transports are guaranteed to report the same numbers. The field
        // list itself lives in `irl_core::stats::FIELDS`.
        let proc_cb = match CString::new(proc_declaration()) {
            Ok(decl) => {
                let state = Arc::clone(&obs_state);
                let lifetime = Arc::clone(&lifetime);
                Some(source.proc_handler().add(
                    &decl,
                    Box::new(move |cd: &mut CallData| {
                        write_stats(cd, &snapshot(&state.lock(), &lifetime));
                    }),
                ))
            }
            Err(_) => {
                irl_error!("Could not build the get_stats declaration; stats are unavailable");
                None
            }
        };

        {
            let mut state = obs_state.lock();
            match state.config.url() {
                Some(url) => {
                    crate::log::log_input_url("Created with URL", url);
                    start_receiver(&mut state, source, &lifetime);
                }
                None => irl_info!("Created with no URL configured"),
            }
        }

        Some(Box::new(Self {
            source,
            lifetime,
            proc_cb,
            obs_state,
        }))
    }

    fn update(&self, settings: &Data<'_>) {
        let mut next = Config::load(settings);
        let mut state = self.obs_state.lock();

        // Editing the source is a request to have it running again.
        state.media_stopped = false;

        if let Some(running) = state.running.as_ref()
            && !state.config.requires_restart(&next)
        {
            // A failed ring resize keeps the old target in force, in the
            // running state *and* in the config this diffs against next time.
            let effective = next.apply_hot(&running.shared);
            next.hot.watermarks = effective;
            state.config = next;
            // close_when_inactive may have just turned on while the source is
            // hidden.
            if !should_run_receiver(&state, self.source) {
                stop_receiver(&mut state, self.source, true);
            }
            return;
        }

        stop_receiver(&mut state, self.source, false);
        state.config = next;
        apply_async_audio_mode(self.source, &state.config);

        // Either the source is not going to run at all, or a restart-forcing
        // edit just dropped the connection: both leave a frame on screen that
        // belongs to a stream that is gone. Clearing is decided against the
        // config that was just installed, not the one being replaced.
        //
        // Ordering matters: this has to happen before the receiver restarts,
        // or the NULL frame could land after the new stream delivered its
        // first one and blank a live picture. Safe to do directly rather than
        // through `VideoChannel::request_clear` because the threads are
        // stopped here, so no frame is in flight to repaint over the clear.
        if !should_run_receiver(&state, self.source) || state.config.hot.clear_on_disconnect {
            self.source.output_video_none();
        }

        start_receiver(&mut state, self.source, &self.lifetime);
    }

    /// Reconnection is handled inside the receiver thread via sleep + retry,
    /// so the only polled work is the one-shot fit. Waiting for a non-zero
    /// source size means the scene item exists and the stream resolution is
    /// known.
    fn video_tick(&self, _seconds: f32) {
        {
            let mut state = self.obs_state.lock();
            if !state.fit_pending || self.source.width() == 0 || self.source.height() == 0 {
                return;
            }
            state.fit_pending = false;
        }
        fit_to_canvas(self.source);
    }

    fn activate(&self) {
        let mut state = self.obs_state.lock();
        if !state.config.close_when_inactive {
            return;
        }
        start_receiver(&mut state, self.source, &self.lifetime);
    }

    fn deactivate(&self) {
        let mut state = self.obs_state.lock();
        if !state.config.close_when_inactive {
            return;
        }
        if !self.source.showing() {
            stop_receiver(&mut state, self.source, true);
        }
    }

    fn show(&self) {
        let mut state = self.obs_state.lock();
        if !state.config.close_when_inactive {
            return;
        }
        start_receiver(&mut state, self.source, &self.lifetime);
    }

    fn hide(&self) {
        let mut state = self.obs_state.lock();
        if !state.config.close_when_inactive {
            return;
        }
        stop_receiver(&mut state, self.source, true);
    }

    // ── Media controls ──
    //
    // A live stream has nothing to seek or pause, so the four callbacks
    // reduce to "run the receiver" and "don't". They exist because
    // OBS_SOURCE_CONTROLLABLE_MEDIA is what makes the source addressable
    // through obs-websocket's TriggerMediaInputAction / GetMediaInputStatus,
    // which is how NOALBS's !fix reconnects a stalled feed, and it is also
    // what puts the source in the media controls dock.

    /// Pause is the only honest reading of "stop receiving" for a live
    /// stream: there is no paused position to resume from, so unpausing
    /// reconnects.
    fn media_play_pause(&self, pause: bool) {
        if pause {
            self.media_stop();
        } else {
            self.media_restart();
        }
    }

    fn media_restart(&self) {
        irl_info!("Media restart requested");

        let mut state = self.obs_state.lock();
        // An explicit restart overrides a previous Stop.
        state.media_stopped = false;
        stop_receiver(&mut state, self.source, true);
        start_receiver(&mut state, self.source, &self.lifetime);

        if state.running.is_some() {
            self.source.media_started();
        }
    }

    fn media_stop(&self) {
        irl_info!("Media stop requested");

        let mut state = self.obs_state.lock();
        state.media_stopped = true;
        stop_receiver(&mut state, self.source, false);
        // Unconditional, unlike a disconnect: the frame is gone because the
        // user asked for the source to stop, which is not what
        // clear_on_disconnect decides. Matches ffmpeg_source_stop().
        self.source.output_video_none();
        // No media_ended here: libobs already fires "media_stopped" for the
        // stop action, and a live stream has no end to report.
    }

    fn media_get_state(&self) -> MediaState {
        let state = self.obs_state.lock();

        if state.config.url().is_none() {
            return MediaState::None;
        }
        // Stopped by the user, or not running because the source is hidden
        // with "Close Stream When Inactive" on. Never ENDED: a live stream
        // has no end to reach.
        let Some(running) = state.running.as_ref() else {
            return MediaState::Stopped;
        };
        if running.shared.flags.reconnecting.load(Relaxed) {
            return MediaState::Opening;
        }

        // Connected, but nothing on screen yet: the first connection attempt
        // is still in avformat_open_input, or the keyframe gate has not
        // opened.
        let playing = running.shared.conn.video_ts_init.load(Relaxed)
            || running.shared.audio_state().primed;
        if playing {
            MediaState::Playing
        } else {
            MediaState::Buffering
        }
    }
}

impl Drop for IrlSource {
    /// `irl_source_destroy`. The frame is not cleared: the source itself is
    /// going away, so there is nothing left to show it on.
    fn drop(&mut self) {
        stop_receiver(&mut self.obs_state.lock(), self.source, false);
        // Explicit for order's sake: the closure the callback owns holds an
        // `Arc<Mutex<ObsState>>`, and dropping it here (before libobs tears
        // down the source's proc handler) is what makes the borrow safe.
        self.proc_cb = None;
    }
}

fn apply_async_audio_mode(source: SourceHandle, config: &Config) {
    source.set_async_unbuffered(config.stream.low_latency_audio);
    source.set_async_decoupled(false);
}

fn should_run_receiver(state: &ObsState, source: SourceHandle) -> bool {
    state.config.url().is_some()
        && !state.media_stopped
        && (!state.config.close_when_inactive || source.showing())
}

/// Start the three workers for a fresh [`Shared`] (which is what replaces the
/// C `reset_runtime_state`: every per-connection field starts zeroed and the
/// lifetime counters are carried over untouched).
fn start_receiver(state: &mut ObsState, source: SourceHandle, lifetime: &Arc<LifetimeStats>) {
    if state.running.is_some() || !should_run_receiver(state, source) {
        return;
    }

    let shared = Shared::new(
        source,
        state.config.stream.clone(),
        state.config.hot,
        Arc::clone(lifetime),
    );
    shared.flags.thread_active.store(true, Relaxed);

    // Spawned in the order the C created them, with the same staged rollback:
    // a thread that never started must not leave the others running.
    let audio = match spawn_worker("irl-audio", Arc::clone(&shared), audio::audio_thread) {
        Ok(handle) => handle,
        Err(err) => {
            irl_error!("Failed to create audio thread: {err}");
            shared.flags.thread_active.store(false, Relaxed);
            return;
        }
    };
    let video = match spawn_worker("irl-video", Arc::clone(&shared), video::video_thread) {
        Ok(handle) => handle,
        Err(err) => {
            irl_error!("Failed to create video thread: {err}");
            shared.flags.thread_active.store(false, Relaxed);
            let _ = audio.join();
            return;
        }
    };
    let receiver = match spawn_worker(
        "irl-receiver",
        Arc::clone(&shared),
        receiver::receiver_thread,
    ) {
        Ok(handle) => handle,
        Err(err) => {
            irl_error!("Failed to create receiver thread: {err}");
            shared.flags.thread_active.store(false, Relaxed);
            shared.video.wake_all();
            let _ = video.join();
            let _ = audio.join();
            return;
        }
    };

    state.running = Some(Running {
        shared,
        receiver,
        audio,
        video,
    });
}

/// Stop the workers and drop the run state.
///
/// `clear_video` asks for the frame to be dropped because the stream stopped,
/// so it is subject to `clear_on_disconnect`. Callers that stop the source
/// outright (no URL, teardown, an explicit media Stop) decide for themselves.
fn stop_receiver(state: &mut ObsState, source: SourceHandle, clear_video: bool) {
    if let Some(running) = state.running.take() {
        // Clearing the flag also trips the FFmpeg interrupt watch, so a
        // receiver blocked in `av_read_frame` returns instead of waiting out
        // its I/O timeout.
        running.shared.flags.thread_active.store(false, Relaxed);
        running.shared.video.wake_all();
        let _ = running.video.join();
        let _ = running.audio.join();
        let _ = running.receiver.join();
        // Whatever the video thread never got to; frees the pinned surfaces.
        running.shared.video.drain();
    }

    if clear_video && state.config.hot.clear_on_disconnect {
        source.output_video_none();
    }
}

// ── Fit to canvas ─────────────────────────────────────────────

/// Size a newly added source to the canvas exactly the way the Fit to Screen
/// menu action does (same `obs_transform_info`, so the result is
/// indistinguishable from pressing it by hand). Fires once, and only for a
/// source that was created without a URL: anything restored from a scene
/// collection already has one, so saved layouts are never touched and
/// upgrading cannot move an existing scene item.
fn fit_to_canvas(source: SourceHandle) {
    let Some(ovi) = obs::scene::get_video_info() else {
        return;
    };

    let fit = TransformInfo {
        pos: (0.0, 0.0),
        rot: 0.0,
        scale: (1.0, 1.0),
        alignment: obs::sys::OBS_ALIGN_LEFT | obs::sys::OBS_ALIGN_TOP,
        bounds_type: BoundsType::ScaleInner,
        bounds_alignment: obs::sys::OBS_ALIGN_CENTER,
        bounds: (ovi.base_width as f32, ovi.base_height as f32),
        crop_to_bounds: false,
    };

    obs::scene::enum_scenes(&mut |scene_source| {
        if let Some(scene) = Scene::from_source(scene_source) {
            scene.enum_items(&mut |item| {
                if std::ptr::eq(item.source().as_ptr(), source.as_ptr()) && !item.is_locked() {
                    let mut info = fit;
                    info.crop_to_bounds = item.bounds_crop();
                    item.set_info2(&info);
                }
                true
            });
        }
        true
    });
}

// ── Stats ─────────────────────────────────────────────────────

/// Snapshot every stat, consistently: the audio state (and, under it, the
/// jitter buffer) is locked once, exactly as the C did.
///
/// With no run in progress the per-connection counters read zero — the C read
/// the same fields after `reset_runtime_state` had zeroed them — while the
/// lifetime counters and the settings-derived flags still report.
fn snapshot(state: &ObsState, lifetime: &LifetimeStats) -> StatsSnapshot {
    let mut snap = StatsSnapshot {
        current_speed: 1.0,
        adaptive_latency_control: state.config.hot.adaptive_speed,
        low_latency_audio: state.config.stream.low_latency_audio,
        video_lead_excess: lifetime.video_lead_excess.load(Relaxed) as i64,
        reconnect_count: lifetime.reconnect_count.load(Relaxed) as i64,
        ..StatsSnapshot::default()
    };

    let Some(running) = state.running.as_ref() else {
        return snap;
    };
    let shared = &running.shared;
    let conn = &shared.conn;

    let audio = shared.audio_state();
    snap.buffer_fill_ms = shared
        .audio_buf()
        .as_ref()
        .map_or(0, |buf| buf.fill_ms() as i64);

    snap.current_speed = conn.current_speed() as f64;
    snap.reconnecting = shared.flags.reconnecting.load(Relaxed);
    snap.total_audio_frames = conn.total_audio_frames.load(Relaxed) as i64;
    snap.total_video_frames = conn.total_video_frames.load(Relaxed) as i64;
    snap.pts_repairs = conn.pts_repairs.load(Relaxed) as i64;
    snap.pts_normalizations = conn.pts_normalizations.load(Relaxed) as i64;
    snap.pts_interpolations = conn.pts_interpolations.load(Relaxed) as i64;
    snap.pts_resets = conn.pts_resets.load(Relaxed) as i64;
    snap.pts_last_gap_ms = conn.pts_last_gap_ms.load(Relaxed) as i64;
    snap.pts_max_gap_ms = conn.pts_max_gap_ms.load(Relaxed) as i64;
    snap.silence_insertions = conn.silence_insertions.load(Relaxed) as i64;
    snap.audio_underruns = conn.audio_underruns.load(Relaxed) as i64;
    snap.audio_resync_skipped_chunks = conn.audio_resync_skipped_chunks.load(Relaxed) as i64;
    snap.audio_hidden_trimmed_chunks = conn.audio_hidden_trimmed_chunks.load(Relaxed) as i64;
    snap.audio_quality_events = conn.audio_quality_events.load(Relaxed) as i64;
    snap.audio_output_restarts = conn.audio_output_restarts.load(Relaxed) as i64;
    snap.obs_lead_ms = conn.last_obs_lead_ns.load(Relaxed) / 1_000_000;
    snap.audio_decoder_flushes = conn.audio_decoder_flushes.load(Relaxed) as i64;
    snap.video_corrupt_frames = conn.video_corrupt_frames.load(Relaxed) as i64;
    snap.video_corrupt_held = conn.video_corrupt_held.load(Relaxed) as i64;
    snap.video_lead_ms = conn.video_lead_ns.load(Relaxed) / 1_000_000;

    // Stream delay: how far behind real time the video output is, computed as
    // wall clock minus the anchored video PTS. Includes SRT latency, decode
    // time and any buffering, which is what makes it the number to watch in a
    // latency overlay.
    if conn.video_ts_init.load(Relaxed) && audio.latest_video_stream_pts_ns != 0 {
        let video_wall_ns = conn.video_sys_base.load(Relaxed) as i64
            + (audio.latest_video_stream_pts_ns - conn.video_pts_base.load(Relaxed));
        snap.stream_delay_ms =
            ((obs::time::gettime_ns() as i64 - video_wall_ns) / 1_000_000).max(0);
    }

    snap
}

/// Write a snapshot into a calldata, walking [`FIELDS`] so the proc
/// declaration and the values can never drift apart. (Public for the
/// integration test that reads them back out.)
pub fn write_stats(cd: &mut CallData, snap: &StatsSnapshot) {
    for ((name, _kind), value) in FIELDS.iter().zip(snap.values()) {
        // Field names are compile-time literals without interior NULs.
        let Ok(name) = CString::new(*name) else {
            continue;
        };
        match value {
            StatValue::Int(v) => cd.set_i64(&name, v),
            StatValue::Float(v) => cd.set_f64(&name, v),
            StatValue::Bool(v) => cd.set_bool(&name, v),
        }
    }
}
