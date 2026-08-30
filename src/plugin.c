/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * plugin.c — OBS module registration
 */

#include <obs-module.h>
#include <libavutil/log.h>
#include <string.h>
#include "../include/irl-source.h"

/*
 * Route the bundled FFmpeg's diagnostics into the OBS log.
 *
 * Without this the plugin's media stack is silent. The FFmpeg it links is
 * statically bound and hidden behind the module's symbol map, so OBS's own
 * av_log callback (installed by obs-ffmpeg for the FFmpeg *OBS* links) can
 * never see it, and the default callback writes to a stderr that a Windows
 * OBS does not have. Every libavformat/libsrt failure — the handshake
 * error, the "no TS sync" probe warning, the reason a URL would not open —
 * went nowhere at all.
 *
 * Issue #28 is what this cost: the report's evidence is two FFmpeg lines
 * copied out of the OBS log, both of which are the *Media Source*'s
 * ("MP:" is media-playback's prefix), because the IRL source had not
 * written a single FFmpeg line for the reporter to find.
 *
 * Warnings and errors only. FFmpeg's AV_LOG_INFO default is per-frame
 * chatter from the decoders and would bury the OBS log on a long stream.
 */
static void irl_ffmpeg_log(void *avcl, int level, const char *fmt, va_list vl)
{
	if (level > av_log_get_level())
		return;

	char line[1024];
	int print_prefix = 1;
	av_log_format_line2(avcl, level, fmt, vl, line, sizeof(line),
			    &print_prefix);

	/* FFmpeg terminates its lines; blog adds its own newline, and a
	 * partial line (print_prefix == 0 on the next call) still reads
	 * fine as its own entry. */
	size_t len = strlen(line);
	while (len > 0 && (line[len - 1] == '\n' || line[len - 1] == '\r'))
		line[--len] = '\0';
	if (len == 0)
		return;

	int obs_level = LOG_INFO;
	if (level <= AV_LOG_ERROR)
		obs_level = LOG_ERROR;
	else if (level <= AV_LOG_WARNING)
		obs_level = LOG_WARNING;

	blog(obs_level, "[irl-source] [ffmpeg] %s", line);
}

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("obs-irl-source", "en-US")

static struct obs_source_info irl_source_info = {
	.id = IRL_SOURCE_ID,
	.type = OBS_SOURCE_TYPE_INPUT,
	.output_flags = OBS_SOURCE_ASYNC_VIDEO | OBS_SOURCE_AUDIO | OBS_SOURCE_DO_NOT_DUPLICATE |
			OBS_SOURCE_CONTROLLABLE_MEDIA,
	.get_name = irl_source_get_name,
	.create = irl_source_create,
	.destroy = irl_source_destroy,
	.update = irl_source_update,
	.activate = irl_source_activate,
	.deactivate = irl_source_deactivate,
	.show = irl_source_show,
	.hide = irl_source_hide,
	.get_defaults = irl_source_get_defaults,
	.get_properties = irl_source_get_properties,
	.video_tick = irl_source_tick,
	.media_play_pause = irl_source_media_play_pause,
	.media_restart = irl_source_media_restart,
	.media_stop = irl_source_media_stop,
	.media_get_state = irl_source_media_get_state,
};

bool obs_module_load(void)
{
	av_log_set_level(AV_LOG_WARNING);
	av_log_set_callback(irl_ffmpeg_log);
	obs_register_source(&irl_source_info);
	return true;
}

/* Runs after every module's obs_module_load(), which is the only point at
 * which obs-websocket is guaranteed to have published its API. See
 * websocket-vendor.c. */
void obs_module_post_load(void)
{
	irl_websocket_vendor_register();
}

const char *obs_module_description(void)
{
	return "IRL Source by irlserver.com — live streaming source with "
	       "jitter buffering, PTS repair, and adaptive latency control";
}

const char *obs_module_author(void)
{
	return "Thomas Lekanger";
}

void obs_module_unload(void)
{
	/* nothing to clean up globally */
}
