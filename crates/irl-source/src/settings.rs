//! Properties UI and defaults (port of `src/settings.c`).

use std::ffi::CString;

use irl_core::{HwDecode, consts};
use obs::{Data, Properties, TextType};

use crate::module_text;
use crate::source::IrlSource;

/// `irl_source_get_defaults`.
///
/// `network_buffer_mb` is deliberately gone: nothing ever read it (the
/// demuxer options use the constant), so it was removed rather than ported.
pub fn defaults(settings: &Data<'_>) {
    settings.set_default_str(c"url", c"");
    settings.set_default_i64(c"reconnect_delay", consts::DEFAULT_RECONNECT_DELAY_S);

    settings.set_default_i64(c"buffer_target_ms", consts::DEFAULT_BUFFER_TARGET_MS);
    settings.set_default_bool(c"adaptive_speed", consts::DEFAULT_ADAPTIVE_SPEED);
    settings.set_default_i64(c"catchup_percent", consts::DEFAULT_CATCHUP_PERCENT);

    settings.set_default_str(c"ffmpeg_options", c"");
    settings.set_default_i64(c"hw_decode", HwDecode::default().as_i64());
    settings.set_default_bool(c"wait_for_keyframe", consts::DEFAULT_WAIT_FOR_KEYFRAME);
    settings.set_default_bool(c"low_latency_audio", consts::DEFAULT_LOW_LATENCY_AUDIO);
    settings.set_default_bool(c"close_when_inactive", consts::DEFAULT_CLOSE_WHEN_INACTIVE);
    settings.set_default_bool(c"clear_on_disconnect", consts::DEFAULT_CLEAR_ON_DISCONNECT);
}

/// `irl_source_get_properties`.
pub fn properties(_instance: Option<&IrlSource>) -> Properties {
    let props = Properties::new();

    // Without this, the dialog calls update() on every keystroke, so typing a
    // URL reopens the stream once per character.
    props.set_flags(obs::sys::OBS_PROPERTIES_DEFER_UPDATE);

    // ── General ──
    props.add_text(c"url", module_text(c"URL"), TextType::Default);
    props.add_int(
        c"reconnect_delay",
        module_text(c"ReconnectDelay"),
        consts::RECONNECT_DELAY_MIN_S,
        consts::RECONNECT_DELAY_MAX_S,
        1,
    );

    // ── Audio Buffer ──
    //
    // IRL uplinks routinely stall for over a second (a field log showed 1.7s
    // gaps with 287 underruns at the 120ms default), and riding those out is
    // the only way to avoid the concealment that inflates the A/V mapping and
    // holds video back with it. High-bitrate senders with deep buffering of
    // their own stall for longer still, which is why the ceiling is
    // `BUFFER_TARGET_MAX_MS` rather than the 2s it was.
    props.add_int(
        c"buffer_target_ms",
        module_text(c"TargetBuffer"),
        consts::BUFFER_TARGET_MIN_MS,
        consts::BUFFER_TARGET_MAX_MS,
        consts::BUFFER_TARGET_STEP_MS,
    );
    props.add_bool(c"adaptive_speed", module_text(c"AdaptiveLatency"));
    // Only meaningful with Adaptive Latency Control on: it is the ceiling on
    // that loop's drain direction.
    props
        .add_int_slider(
            c"catchup_percent",
            module_text(c"CatchUpSpeed"),
            consts::CATCHUP_PERCENT_MIN,
            consts::CATCHUP_PERCENT_MAX,
            1,
        )
        .set_suffix(c"%");
    props.add_text(
        c"audio_buffer_help",
        module_text(c"AudioBufferHelp"),
        TextType::Info,
    );

    // ── Advanced ──
    props.add_text(
        c"ffmpeg_options",
        module_text(c"FFmpegOptions"),
        TextType::Default,
    );

    let hw = props.add_int_list(c"hw_decode", module_text(c"HardwareDecode"));
    hw.add(module_text(c"HardwareDecode.Auto"), HwDecode::Auto.as_i64());
    hw.add(module_text(c"HardwareDecode.Off"), HwDecode::Off.as_i64());
    #[cfg(any(windows, target_os = "linux"))]
    hw.add(
        module_text(c"HardwareDecode.NVDEC"),
        HwDecode::Nvdec.as_i64(),
    );

    props.add_bool(c"wait_for_keyframe", module_text(c"WaitForKeyframe"));
    props.add_bool(c"low_latency_audio", module_text(c"LowLatencyAudio"));
    props.add_bool(c"clear_on_disconnect", module_text(c"ClearOnDisconnect"));
    props.add_bool(c"close_when_inactive", module_text(c"CloseWhenInactive"));
    props.add_text(
        c"advanced_help",
        module_text(c"AdvancedHelp"),
        TextType::Info,
    );

    // ── About ──
    //
    // A textual replace rather than a format string: the template comes from
    // a locale file, and a translation that drops or mistypes the token
    // should render oddly, not read the stack.
    let about = module_text(c"About")
        .to_string_lossy()
        .replace("%1", crate::PLUGIN_VERSION);
    if let Ok(about) = CString::new(about) {
        props.add_text(c"about_info", &about, TextType::Info);
    }

    props
}
