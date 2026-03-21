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
#include "../include/irl-source.h"

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("obs-irl-source", "en-US")

static struct obs_source_info irl_source_info = {
	.id = "irl_source",
	.type = OBS_SOURCE_TYPE_INPUT,
	.output_flags = OBS_SOURCE_AUDIO | OBS_SOURCE_ASYNC_VIDEO |
			OBS_SOURCE_DO_NOT_DUPLICATE,
	.get_name = irl_source_get_name,
	.create = irl_source_create,
	.destroy = irl_source_destroy,
	.update = irl_source_update,
	.get_defaults = irl_source_get_defaults,
	.get_properties = irl_source_get_properties,
	.video_tick = irl_source_tick,
	.audio_render = irl_audio_render,
};

bool obs_module_load(void)
{
	obs_register_source(&irl_source_info);
	return true;
}

const char *obs_module_description(void)
{
	return "IRL Source by irlserver.com — live streaming source with "
	       "jitter buffering, PTS repair, and adaptive playback speed";
}

const char *obs_module_author(void)
{
	return "Thomas Lekanger";
}

void obs_module_unload(void)
{
	/* nothing to clean up globally */
}
