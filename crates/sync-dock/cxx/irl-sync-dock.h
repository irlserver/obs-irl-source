/*
 * obs-irl-source — IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * irl-sync-dock.h — the C ABI between the Qt dock (sync-dock.cpp) and the
 * Rust plugin (crates/sync-dock/src/lib.rs).
 *
 * The dock is the C plugin's, kept as C++ because it is the one piece of the
 * plugin that has to be a QWidget. It knows nothing about the Rust side
 * except this table of functions, which Rust fills at registration, and the
 * plain structs those functions read and write. Every struct here is mirrored
 * field for field by a #[repr(C)] struct in lib.rs; irl_sync_dock_abi_sizes()
 * lets the Rust side check the two agree before it hands anything over.
 *
 * No libobs header is included, on purpose: the Rust build needs none, and
 * the two frontend symbols the dock uses are resolved at runtime.
 */

#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped when anything in this header changes shape. The Rust side passes
 * the version it was built for; a mismatch refuses to register. */
#define IRL_SYNC_DOCK_ABI_VERSION 1

/* Sources the registry can hold; the dock reads them into a stack array. */
#define IRL_SYNC_MAX_SOURCES 64

/* Values match irl_core::sync::SyncStatus in declaration order. */
enum irl_sync_status {
	IRL_SYNC_OFF = 0,
	IRL_SYNC_NO_TIMECODE = 1,
	IRL_SYNC_STALE = 2,
	IRL_SYNC_TOO_SLOW = 3,
	IRL_SYNC_ACQUIRING = 4,
	IRL_SYNC_LOCKED = 5,
};

/* Values match irl_core::sync::TcReason in declaration order. */
enum irl_sync_tc_reason {
	IRL_SYNC_TC_OK = 0,
	IRL_SYNC_TC_NO_CLOCK = 1,
	IRL_SYNC_TC_CODEC = 2,
	IRL_SYNC_TC_ABSENT = 3,
};

enum irl_dock_log_level {
	IRL_DOCK_LOG_INFO = 0,
	IRL_DOCK_LOG_WARNING = 1,
	IRL_DOCK_LOG_ERROR = 2,
};

struct irl_timecode {
	uint8_t hours;
	uint8_t minutes;
	uint8_t seconds;
	uint16_t n_frames;
};

struct irl_sync_snapshot {
	enum irl_sync_status status;
	enum irl_sync_tc_reason tc_reason;
	bool have_timecode;
	struct irl_timecode tc;
	int64_t latency_ms;
	int64_t latency_peak_ms;
	int64_t added_ms;
	int64_t error_ms;
	int64_t required_offset_ms;
};

struct irl_sync_entry {
	char source_name[256];
	bool sync_enabled;
	struct irl_sync_snapshot snap;
};

struct irl_sync_config {
	bool enabled;
	int32_t offset_ms;
	char ntp_server[128];
};

struct irl_ntp_status {
	bool synced;
	int64_t offset_ns;
	int64_t rtt_ns;
	uint64_t age_ns;
	char server[128];
	char primary[128];
	bool burst_mode;
	bool fallback_active;
	bool drift_valid;
	double drift_ppm;
};

/* What the dock calls. Every function pointer is required except `log`.
 * Strings handed in (`set_ntp_server`, `log`) are only read during the
 * call; strings handed out are copied into the caller's fixed buffers,
 * NUL-terminated and truncated. */
struct irl_sync_dock_api {
	uint32_t abi_version;
	int32_t min_offset_ms;
	int32_t max_offset_ms;
	int32_t offset_step_ms;
	/* Lives for the whole process. */
	const char *default_ntp_server;

	void (*config_get)(struct irl_sync_config *out);
	void (*set_enabled)(bool enabled);
	void (*set_offset_ms)(int32_t offset_ms);
	void (*set_ntp_server)(const char *host);
	void (*ntp_status)(struct irl_ntp_status *out);
	bool (*utc_now_ns)(int64_t *utc_ns);
	size_t (*collect)(struct irl_sync_entry *entries, size_t max);
	void (*log)(enum irl_dock_log_level level, const char *message);
};

/* sizeof() of each shared struct as the C++ side was compiled, so the Rust
 * side can refuse a mismatch instead of reading garbage. */
struct irl_sync_dock_abi {
	size_t timecode;
	size_t snapshot;
	size_t entry;
	size_t config;
	size_t ntp_status;
	size_t api;
};

void irl_sync_dock_abi_sizes(struct irl_sync_dock_abi *out);

/* Create the dock and hand it to the frontend. Copies `api`. Returns whether
 * the dock is now registered (false when the ABI does not match, the frontend
 * is not available, or the frontend refused it); every failure is logged
 * through `api->log`. Idempotent. */
bool irl_sync_dock_register(const struct irl_sync_dock_api *api);

/* Remove the dock. Safe at shutdown as well as at a live unload. */
void irl_sync_dock_unregister(void);

#ifdef __cplusplus
}
#endif
