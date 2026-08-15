/*
 * obs-irl-source: IRL streaming source plugin for OBS
 * https://irlserver.com
 *
 * Copyright (C) 2026 Thomas Lekanger
 *
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * The plugin's mutexes, condition variables and worker threads.
 *
 * This exists because the plugin must NOT call pthread_* by name on
 * Windows. librist ships contrib/pthread-shim.c, which defines external
 * pthread_mutex_init/lock/unlock/destroy, pthread_cond_*, pthread_create
 * and pthread_join for MSVC builds, and librist.lib is linked into this
 * module ahead of w32-pthreads. Every pthread_* call in the plugin
 * therefore resolved to librist's shim, while the declarations came from
 * w32-pthreads' pthread.h:
 *
 *   - w32-pthreads: pthread_mutex_t is a pointer (8 bytes)
 *   - librist shim: pthread_mutex_t is a CRITICAL_SECTION (40 bytes)
 *
 * so pthread_mutex_init() wrote 40 bytes into an 8-byte struct field and
 * shredded whatever followed it (thread_active, the video queue lock, the
 * audio ring buffer's bookkeeping). pthread_t had the same problem in
 * the other direction: w32-pthreads' 16-byte struct is passed by hidden
 * pointer, which the shim's HANDLE-by-value pthread_join then waited on,
 * so joins returned immediately without waiting for the thread.
 *
 * Owning these primitives makes the plugin independent of which pthread
 * implementation happens to win the link. Keep it that way: do not
 * reintroduce direct pthread_* calls in plugin code.
 */

#pragma once

#ifdef _WIN32

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <process.h>
#include <stdint.h>
#include <stdlib.h>

typedef CRITICAL_SECTION irl_mutex_t;
typedef CONDITION_VARIABLE irl_cond_t;
typedef HANDLE irl_thread_t;

static inline int irl_mutex_init(irl_mutex_t *m)
{
	InitializeCriticalSection(m);
	return 0;
}

static inline void irl_mutex_destroy(irl_mutex_t *m)
{
	DeleteCriticalSection(m);
}

static inline void irl_mutex_lock(irl_mutex_t *m)
{
	EnterCriticalSection(m);
}

static inline void irl_mutex_unlock(irl_mutex_t *m)
{
	LeaveCriticalSection(m);
}

static inline int irl_cond_init(irl_cond_t *c)
{
	InitializeConditionVariable(c);
	return 0;
}

static inline void irl_cond_destroy(irl_cond_t *c)
{
	/* Condition variables carry no OS resources to release. */
	(void)c;
}

static inline void irl_cond_wait(irl_cond_t *c, irl_mutex_t *m)
{
	SleepConditionVariableCS(c, m, INFINITE);
}

/* Wait until signalled or `timeout_ms` elapses. Spurious and early wakeups
 * are permitted on every backend, so callers must re-check their predicate
 * and recompute the remaining time rather than assume the full interval
 * passed. */
static inline void irl_cond_timedwait(irl_cond_t *c, irl_mutex_t *m,
				      uint32_t timeout_ms)
{
	SleepConditionVariableCS(c, m, (DWORD)timeout_ms);
}

static inline void irl_cond_signal(irl_cond_t *c)
{
	WakeConditionVariable(c);
}

static inline void irl_cond_broadcast(irl_cond_t *c)
{
	WakeAllConditionVariable(c);
}

struct irl_thread_start {
	void *(*fn)(void *);
	void *arg;
};

static inline unsigned __stdcall irl_thread_trampoline(void *param)
{
	struct irl_thread_start start = *(struct irl_thread_start *)param;
	free(param);
	start.fn(start.arg);
	return 0;
}

/* Returns 0 on success, non-zero on failure (same convention as
 * pthread_create, which is what the call sites already check). */
static inline int irl_thread_create(irl_thread_t *t, void *(*fn)(void *),
				    void *arg)
{
	struct irl_thread_start *start = malloc(sizeof(*start));
	if (!start)
		return -1;

	start->fn = fn;
	start->arg = arg;

	/* _beginthreadex, not CreateThread: the thread runs CRT code
	 * (FFmpeg, the plugin's own allocations) and needs per-thread CRT
	 * state set up and torn down. */
	uintptr_t handle = _beginthreadex(NULL, 0, irl_thread_trampoline,
					  start, 0, NULL);
	if (!handle) {
		free(start);
		return -1;
	}

	*t = (irl_thread_t)handle;
	return 0;
}

static inline void irl_thread_join(irl_thread_t *t)
{
	if (!*t)
		return;

	WaitForSingleObject(*t, INFINITE);
	CloseHandle(*t);
	*t = NULL;
}

#else /* !_WIN32 */

#include <pthread.h>
#include <stdint.h>
#include <time.h>

typedef pthread_mutex_t irl_mutex_t;
typedef pthread_cond_t irl_cond_t;
typedef pthread_t irl_thread_t;

static inline int irl_mutex_init(irl_mutex_t *m)
{
	pthread_mutexattr_t attr;
	if (pthread_mutexattr_init(&attr) != 0)
		return pthread_mutex_init(m, NULL);
	pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE);
	int ret = pthread_mutex_init(m, &attr);
	pthread_mutexattr_destroy(&attr);
	return ret;
}

static inline void irl_mutex_destroy(irl_mutex_t *m)
{
	pthread_mutex_destroy(m);
}

static inline void irl_mutex_lock(irl_mutex_t *m)
{
	pthread_mutex_lock(m);
}

static inline void irl_mutex_unlock(irl_mutex_t *m)
{
	pthread_mutex_unlock(m);
}

static inline int irl_cond_init(irl_cond_t *c)
{
#if defined(__APPLE__)
	/* macOS has no pthread_condattr_setclock. Its timed wait below is
	 * pthread_cond_timedwait_relative_np, which takes an interval rather
	 * than a deadline and so is already immune to clock changes. */
	return pthread_cond_init(c, NULL);
#else
	/* Bind the condvar to CLOCK_MONOTONIC. pthread_cond_timedwait takes an
	 * absolute deadline against the condvar's clock, and the default is
	 * CLOCK_REALTIME — an NTP step or a manual clock change would then
	 * stretch or collapse a video pacing wait. */
	pthread_condattr_t attr;
	if (pthread_condattr_init(&attr) != 0)
		return pthread_cond_init(c, NULL);
	pthread_condattr_setclock(&attr, CLOCK_MONOTONIC);
	int ret = pthread_cond_init(c, &attr);
	pthread_condattr_destroy(&attr);
	return ret;
#endif
}

static inline void irl_cond_destroy(irl_cond_t *c)
{
	pthread_cond_destroy(c);
}

static inline void irl_cond_wait(irl_cond_t *c, irl_mutex_t *m)
{
	pthread_cond_wait(c, m);
}

/* Wait until signalled or `timeout_ms` elapses. Spurious and early wakeups
 * are permitted on every backend, so callers must re-check their predicate
 * and recompute the remaining time rather than assume the full interval
 * passed. */
static inline void irl_cond_timedwait(irl_cond_t *c, irl_mutex_t *m,
				      uint32_t timeout_ms)
{
#if defined(__APPLE__)
	struct timespec rel;
	rel.tv_sec = (time_t)(timeout_ms / 1000u);
	rel.tv_nsec = (long)(timeout_ms % 1000u) * 1000000L;
	pthread_cond_timedwait_relative_np(c, m, &rel);
#else
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	ts.tv_sec += (time_t)(timeout_ms / 1000u);
	ts.tv_nsec += (long)(timeout_ms % 1000u) * 1000000L;
	if (ts.tv_nsec >= 1000000000L) {
		ts.tv_sec += 1;
		ts.tv_nsec -= 1000000000L;
	}
	pthread_cond_timedwait(c, m, &ts);
#endif
}

static inline void irl_cond_signal(irl_cond_t *c)
{
	pthread_cond_signal(c);
}

static inline void irl_cond_broadcast(irl_cond_t *c)
{
	pthread_cond_broadcast(c);
}

static inline int irl_thread_create(irl_thread_t *t, void *(*fn)(void *),
				    void *arg)
{
	return pthread_create(t, NULL, fn, arg);
}

static inline void irl_thread_join(irl_thread_t *t)
{
	pthread_join(*t, NULL);
}

#endif /* _WIN32 */
