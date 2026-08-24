/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * websocket-vendor.c — obs-websocket vendor extension
 *
 * Exposes the per-source stats over obs-websocket so an overlay, a bot or
 * an IRL dashboard can read them from another machine, without the Lua or
 * Python script that the proc_handler path requires.
 *
 * Clients reach these through obs-websocket's own CallVendorRequest:
 *
 *   {"vendorName": "obs-irl-source", "requestType": "GetStats",
 *    "requestData": {"source_name": "IRL Source"}}
 *
 * The stats themselves are not read out of struct irl_source here. This
 * file calls the source's existing "get_stats" proc_handler, the same
 * entry point scripts use, so both transports are guaranteed to report the
 * same numbers, and the audio_state_lock snapshot stays in one place
 * (irl-source.c). obs-websocket runs request callbacks on its own thread,
 * and holding a source reference across the call keeps the source alive
 * for its duration.
 */

#include <assert.h>
#include <string.h>

#include <obs-module.h>

#include "../include/irl-source.h"
#include "../third_party/obs-websocket-api.h"

#define IRL_VENDOR_NAME "obs-irl-source"

/* Bumped when a request is added or a response field changes meaning, so a
 * client can feature-detect instead of probing. */
#define IRL_VENDOR_API_VERSION 1

#define IRL_ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))

static obs_websocket_vendor irl_vendor = NULL;

/* ── Stats field table ────────────────────────────────────── */

enum irl_stat_type {
	IRL_STAT_INT,
	IRL_STAT_FLOAT,
	IRL_STAT_BOOL,
};

struct irl_stat_field {
	const char *name;
	enum irl_stat_type type;
};

/* calldata is a typed blob that cannot be enumerated, so the fields to copy
 * out have to be named. Keep this in sync with the get_stats declaration in
 * irl_source_create() (irl-source.c) and the stats table in README.md — the
 * names here are the JSON keys clients see. */
static const struct irl_stat_field irl_stat_fields[] = {
	{"buffer_fill_ms", IRL_STAT_INT},
	{"current_speed", IRL_STAT_FLOAT},
	{"adaptive_latency_control", IRL_STAT_BOOL},
	{"reconnecting", IRL_STAT_BOOL},
	{"total_audio_frames", IRL_STAT_INT},
	{"total_video_frames", IRL_STAT_INT},
	{"pts_repairs", IRL_STAT_INT},
	{"pts_normalizations", IRL_STAT_INT},
	{"pts_interpolations", IRL_STAT_INT},
	{"pts_resets", IRL_STAT_INT},
	{"pts_last_gap_ms", IRL_STAT_INT},
	{"pts_max_gap_ms", IRL_STAT_INT},
	{"silence_insertions", IRL_STAT_INT},
	{"audio_underruns", IRL_STAT_INT},
	{"audio_resync_skipped_chunks", IRL_STAT_INT},
	{"audio_hidden_trimmed_chunks", IRL_STAT_INT},
	{"audio_quality_events", IRL_STAT_INT},
	{"audio_output_restarts", IRL_STAT_INT},
	{"obs_lead_ms", IRL_STAT_INT},
	{"audio_decoder_flushes", IRL_STAT_INT},
	{"video_decoder_flushes", IRL_STAT_INT},
	{"video_corrupt_frames", IRL_STAT_INT},
	{"video_corrupt_held", IRL_STAT_INT},
	{"video_lead_ms", IRL_STAT_INT},
	{"video_lead_excess", IRL_STAT_INT},
	{"stream_delay_ms", IRL_STAT_INT},
	{"low_latency_audio", IRL_STAT_BOOL},
	{"reconnect_count", IRL_STAT_INT},
};

static void stats_to_obs_data(const calldata_t *cd, obs_data_t *out)
{
	for (size_t i = 0; i < IRL_ARRAY_SIZE(irl_stat_fields); i++) {
		const struct irl_stat_field *f = &irl_stat_fields[i];

		switch (f->type) {
		case IRL_STAT_INT: {
			long long v = 0;
			calldata_get_int(cd, f->name, &v);
			obs_data_set_int(out, f->name, v);
			break;
		}
		case IRL_STAT_FLOAT: {
			double v = 0.0;
			calldata_get_float(cd, f->name, &v);
			obs_data_set_double(out, f->name, v);
			break;
		}
		case IRL_STAT_BOOL: {
			bool v = false;
			calldata_get_bool(cd, f->name, &v);
			obs_data_set_bool(out, f->name, v);
			break;
		}
		}
	}
}

/* ── Response helpers ─────────────────────────────────────── */

/* Vendor requests have no status code of their own: obs-websocket reports
 * RequestStatus::Success as long as the callback ran, and hands the client
 * whatever the callback put in response_data. So the outcome is carried in
 * the payload. Every response has "success"; failures add "error". */
static void respond_error(obs_data_t *response_data, const char *message)
{
	obs_data_set_bool(response_data, "success", false);
	obs_data_set_string(response_data, "error", message);
}

/* ── Source lookup ────────────────────────────────────────── */

static bool source_is_irl(obs_source_t *source)
{
	const char *id = obs_source_get_unversioned_id(source);
	return id && strcmp(id, IRL_SOURCE_ID) == 0;
}

struct irl_source_search {
	obs_source_t *first; /* strong reference, released by the caller */
	int count;
};

static bool enum_irl_sources(void *param, obs_source_t *source)
{
	struct irl_source_search *search = param;

	if (!source_is_irl(source))
		return true;

	search->count++;
	if (!search->first)
		/* Not obs_source_get_ref()'s only job here: it returns NULL
		 * for a source already on its way out, which is exactly the
		 * one we must not hand back. */
		search->first = obs_source_get_ref(source);

	return true;
}

/* Resolves which source the request is about. "source_name" names one
 * explicitly; without it, a scene collection holding exactly one IRL source
 * resolves to that source, because that is the common setup and it saves
 * every client a GetSourceList round trip first.
 *
 * Returns a strong reference the caller must release, or NULL after writing
 * the failure into response_data. */
static obs_source_t *resolve_source(obs_data_t *request_data,
				    obs_data_t *response_data)
{
	/* Accept the obs-websocket house style as an alias: core requests all
	 * take "sourceName", so that is what a client reaches for first. */
	const char *name = obs_data_get_string(request_data, "source_name");
	if (!name || !*name)
		name = obs_data_get_string(request_data, "sourceName");

	if (name && *name) {
		obs_source_t *source = obs_get_source_by_name(name);
		if (!source) {
			respond_error(response_data, "No source by that name");
			return NULL;
		}
		if (!source_is_irl(source)) {
			obs_source_release(source);
			respond_error(response_data,
				      "That source is not an IRL Source");
			return NULL;
		}
		return source;
	}

	struct irl_source_search search = {0};
	obs_enum_sources(enum_irl_sources, &search);

	if (search.count == 0) {
		respond_error(response_data, "No IRL Source exists");
		return NULL;
	}
	if (search.count > 1) {
		if (search.first)
			obs_source_release(search.first);
		respond_error(
			response_data,
			"More than one IRL Source exists; pass source_name (see GetSourceList)");
		return NULL;
	}
	if (!search.first) {
		respond_error(response_data, "IRL Source is being destroyed");
		return NULL;
	}

	return search.first;
}

/* ── Requests ─────────────────────────────────────────────── */

static void vendor_get_stats(obs_data_t *request_data,
			     obs_data_t *response_data, void *priv_data)
{
	UNUSED_PARAMETER(priv_data);

	obs_source_t *source = resolve_source(request_data, response_data);
	if (!source)
		return;

	calldata_t cd;
	calldata_init(&cd);

	proc_handler_t *ph = obs_source_get_proc_handler(source);
	bool called = ph && proc_handler_call(ph, "get_stats", &cd);

	if (called) {
		obs_data_set_string(response_data, "source_name",
				    obs_source_get_name(source));
		stats_to_obs_data(&cd, response_data);
		obs_data_set_bool(response_data, "success", true);
	} else {
		respond_error(response_data, "Source did not answer get_stats");
	}

	calldata_free(&cd);
	obs_source_release(source);
}

static bool enum_source_list(void *param, obs_source_t *source)
{
	obs_data_array_t *array = param;

	if (!source_is_irl(source))
		return true;

	obs_data_t *entry = obs_data_create();
	obs_data_set_string(entry, "source_name", obs_source_get_name(source));
	/* Deliberately no URL: it can carry an SRT passphrase or a stream key,
	 * and this list is readable by every connected websocket client. */
	obs_data_set_bool(entry, "active", obs_source_active(source));
	obs_data_set_bool(entry, "showing", obs_source_showing(source));
	obs_data_array_push_back(array, entry);
	obs_data_release(entry);

	return true;
}

static void vendor_get_source_list(obs_data_t *request_data,
				   obs_data_t *response_data, void *priv_data)
{
	UNUSED_PARAMETER(request_data);
	UNUSED_PARAMETER(priv_data);

	obs_data_array_t *array = obs_data_array_create();
	obs_enum_sources(enum_source_list, array);

	obs_data_set_array(response_data, "sources", array);
	obs_data_array_release(array);
	obs_data_set_bool(response_data, "success", true);
}

static void vendor_get_version(obs_data_t *request_data,
			       obs_data_t *response_data, void *priv_data)
{
	UNUSED_PARAMETER(request_data);
	UNUSED_PARAMETER(priv_data);

	obs_data_set_string(response_data, "plugin_version",
			    OBS_IRL_SOURCE_VERSION);
	obs_data_set_int(response_data, "vendor_api_version",
			 IRL_VENDOR_API_VERSION);
	obs_data_set_int(response_data, "obs_websocket_api_version",
			 (long long)obs_websocket_get_api_version());
	obs_data_set_bool(response_data, "success", true);
}

/* ── Registration ─────────────────────────────────────────── */

static const struct {
	const char *type;
	obs_websocket_request_callback_function callback;
} irl_vendor_requests[] = {
	{"GetStats", vendor_get_stats},
	{"GetSourceList", vendor_get_source_list},
	{"GetVersion", vendor_get_version},
};

/*
 * Must run from obs_module_post_load(): obs-websocket publishes the global
 * proc this goes through from its own obs_module_load(), and module load
 * order between plugins is not defined. Every module's load has finished by
 * the time any post_load runs.
 *
 * There is no matching teardown, by design. The API has no vendor
 * unregister call — a registration is meant to last for the life of the
 * process — and the request-unregister proc would have to be called during
 * module unload at shutdown, where obs-websocket may already have destroyed
 * the proc handler and the vendor object this holds.
 */
void irl_websocket_vendor_register(void)
{
	if (irl_vendor)
		return;

	irl_vendor = obs_websocket_register_vendor(IRL_VENDOR_NAME);
	if (!irl_vendor) {
		/* The normal case on an OBS without obs-websocket enabled.
		 * Nothing else in the plugin depends on it. */
		blog(LOG_INFO,
		     "[irl-source] obs-websocket not available; vendor requests disabled");
		return;
	}

	for (size_t i = 0; i < IRL_ARRAY_SIZE(irl_vendor_requests); i++) {
		if (!obs_websocket_vendor_register_request(
			    irl_vendor, irl_vendor_requests[i].type,
			    irl_vendor_requests[i].callback, NULL)) {
			blog(LOG_WARNING,
			     "[irl-source] Failed to register obs-websocket vendor request '%s'",
			     irl_vendor_requests[i].type);
		}
	}

	blog(LOG_INFO,
	     "[irl-source] Registered obs-websocket vendor '%s' (obs-websocket API v%u)",
	     IRL_VENDOR_NAME, obs_websocket_get_api_version());
}
