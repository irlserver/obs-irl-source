/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * settings.c — OBS properties UI for the IRL source
 */

#include <util/dstr.h>

#include "../include/irl-source.h"

/* ── Defaults ─────────────────────────────────────────────── */

/* OBS get_defaults callback: sets default values for all settings. */
void irl_source_get_defaults(obs_data_t *settings)
{
	obs_data_set_default_string(settings, "url", "");
	obs_data_set_default_int(settings, "reconnect_delay",
				 IRL_DEFAULT_RECONNECT_DELAY);
	obs_data_set_default_int(settings, "network_buffer_mb",
				 IRL_DEFAULT_NETWORK_BUFFER_MB);

	obs_data_set_default_int(settings, "buffer_target_ms",
				 IRL_DEFAULT_BUFFER_TARGET_MS);
	obs_data_set_default_bool(settings, "adaptive_speed",
				  IRL_DEFAULT_ADAPTIVE_SPEED);
	obs_data_set_default_int(settings, "catchup_percent",
				 IRL_DEFAULT_CATCHUP_PERCENT);

	obs_data_set_default_string(settings, "ffmpeg_options", "");
	obs_data_set_default_int(settings, "hw_decode", IRL_DEFAULT_HW_DECODE);
	obs_data_set_default_bool(settings, "wait_for_keyframe",
				  IRL_DEFAULT_WAIT_KEYFRAME);
	obs_data_set_default_bool(settings, "low_latency_audio",
				  IRL_DEFAULT_LOW_LATENCY_AUDIO);
	obs_data_set_default_bool(settings, "close_when_inactive",
				  IRL_DEFAULT_CLOSE_WHEN_INACTIVE);
	obs_data_set_default_bool(settings, "clear_on_disconnect",
				  IRL_DEFAULT_CLEAR_ON_DISCONNECT);
}

/* ── Properties ───────────────────────────────────────────── */

/* OBS get_properties callback: builds the source settings UI. */
obs_properties_t *irl_source_get_properties(void *data)
{
	UNUSED_PARAMETER(data);

	obs_properties_t *props = obs_properties_create();

	/* Without this, the dialog calls update() on every keystroke, so
	 * typing a URL reopens the stream once per character. */
	obs_properties_set_flags(props, OBS_PROPERTIES_DEFER_UPDATE);

	/* ── General ───────────────────────────────────────── */

	obs_properties_add_text(props, "url", obs_module_text("URL"),
				OBS_TEXT_DEFAULT);
	obs_properties_add_int(props, "reconnect_delay",
			       obs_module_text("ReconnectDelay"), 1, 60, 1);

	/* ── Audio Buffer ──────────────────────────────────── */

	/* IRL uplinks routinely stall for over a second (a field log showed
	 * 1.7s gaps with 287 underruns at the 120ms default), and riding those
	 * out is the only way to avoid the concealment that inflates the A/V
	 * mapping and holds video back with it. High-bitrate senders with deep
	 * buffering of their own stall for longer still, which is why the
	 * ceiling is IRL_BUFFER_TARGET_MAX_MS rather than the 2s it was. */
	obs_properties_add_int(props, "buffer_target_ms",
			       obs_module_text("TargetBuffer"),
			       IRL_BUFFER_TARGET_MIN_MS,
			       IRL_BUFFER_TARGET_MAX_MS, 10);
	obs_properties_add_bool(props, "adaptive_speed",
				obs_module_text("AdaptiveLatency"));
	/* Only meaningful with Adaptive Latency Control on: it is the ceiling
	 * on that loop's drain direction. */
	obs_property_t *catchup = obs_properties_add_int_slider(
		props, "catchup_percent", obs_module_text("CatchUpSpeed"),
		IRL_CATCHUP_PERCENT_MIN, IRL_CATCHUP_PERCENT_MAX, 1);
	obs_property_int_set_suffix(catchup, "%");
	obs_properties_add_text(props, "audio_buffer_help",
				obs_module_text("AudioBufferHelp"),
				OBS_TEXT_INFO);

	/* ── Advanced ──────────────────────────────────────── */

	obs_properties_add_text(props, "ffmpeg_options",
				obs_module_text("FFmpegOptions"),
				OBS_TEXT_DEFAULT);

	obs_property_t *hw = obs_properties_add_list(
		props, "hw_decode", obs_module_text("HardwareDecode"),
		OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_INT);
	obs_property_list_add_int(hw, obs_module_text("HardwareDecode.Auto"),
				  IRL_HW_DECODE_AUTO);
	obs_property_list_add_int(hw, obs_module_text("HardwareDecode.Off"),
				  IRL_HW_DECODE_OFF);
#if defined(_WIN32) || defined(__linux__)
	obs_property_list_add_int(hw,
				  obs_module_text("HardwareDecode.NVDEC"),
				  IRL_HW_DECODE_NVDEC);
#endif

	obs_properties_add_bool(props, "wait_for_keyframe",
				obs_module_text("WaitForKeyframe"));
	obs_properties_add_bool(props, "low_latency_audio",
				obs_module_text("LowLatencyAudio"));
	obs_properties_add_bool(props, "clear_on_disconnect",
				obs_module_text("ClearOnDisconnect"));
	obs_properties_add_bool(props, "close_when_inactive",
				obs_module_text("CloseWhenInactive"));
	obs_properties_add_text(props, "advanced_help",
				obs_module_text("AdvancedHelp"), OBS_TEXT_INFO);

	/* ── About ─────────────────────────────────────────── */

	/* dstr_replace rather than a printf format: the template comes from a
	 * locale file, and a translation that drops or mistypes the token
	 * should render oddly, not read the stack. */
	struct dstr about = {0};
	dstr_copy(&about, obs_module_text("About"));
	dstr_replace(&about, "%1", OBS_IRL_SOURCE_VERSION);
	obs_properties_add_text(props, "about_info", about.array,
				OBS_TEXT_INFO);
	dstr_free(&about);

	return props;
}
