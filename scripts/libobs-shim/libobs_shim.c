/*
 * A stand-in for the handful of libobs functions the plugin's pacing and
 * network-simulation tests call, so those tests can run on a machine with no
 * libobs to link against (a Mac, in practice). Nothing here needs a running
 * libobs: a monotonic clock, a canvas tick the tests override anyway, a log
 * sink and a colour matrix lookup whose values no test asserts on.
 *
 * scripts/test-with-libobs-shim.sh builds this into a dylib, links it into
 * the test binaries it covers and refuses to run a binary that imports a
 * libobs symbol not defined here, so a new libobs call in the code under test
 * shows up as a named symbol rather than a null-pointer crash.
 */
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

uint64_t os_gettime_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* A 60fps canvas, what a default OBS install reports. */
uint64_t obs_get_frame_interval_ns(void)
{
	return 16666667ull;
}

void blog(int log_level, const char *format, ...)
{
	va_list args;
	va_start(args, format);
	fprintf(stderr, "[blog %d] ", log_level);
	vfprintf(stderr, format, args);
	fputc('\n', stderr);
	va_end(args);
}

bool video_format_get_parameters_for_format(int color_space, int range, int format,
					    float matrix[16], float range_min[3],
					    float range_max[3])
{
	(void)color_space;
	(void)range;
	(void)format;
	for (int i = 0; i < 16; i++)
		matrix[i] = (i % 5 == 0) ? 1.0f : 0.0f;
	for (int i = 0; i < 3; i++) {
		range_min[i] = 0.0f;
		range_max[i] = 1.0f;
	}
	return true;
}
