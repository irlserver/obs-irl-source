#!/usr/bin/env bash
# Run the libobs-dependent integration tests where there is no libobs.
#
# The test binaries resolve libobs at load time (`-undefined dynamic_lookup`
# on macOS), so on a machine without it every call into libobs lands on a null
# pointer and the binary dies with SIGSEGV before the first assertion. The
# tests listed below only reach four libobs functions, none of which needs a
# running libobs, so this links the stand-in in scripts/libobs-shim/ into those
# binaries and runs them. Linux links the real libobs through libobs-dev, and
# CI runs the tests there; this is for development on a Mac.
#
# Extra arguments go to the test harness, so `make test-shim ARGS=pacing` runs
# the tests whose name contains "pacing".
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" != Darwin ]]; then
	echo "test-with-libobs-shim: macOS only; on Linux install libobs-dev and run cargo test" >&2
	exit 1
fi

# The tests whose libobs imports the shim covers. shell_stats is not one of
# them: calldata is real libobs bookkeeping and needs the real library.
TESTS=(audio_core network_sim video_pipeline)

SHIM_SRC=scripts/libobs-shim/libobs_shim.c
SHIM_DIR=target/libobs-shim
SHIM="$PWD/$SHIM_DIR/libobs_shim.dylib"
mkdir -p "$SHIM_DIR"
if [[ ! -f "$SHIM" || "$SHIM_SRC" -nt "$SHIM" ]]; then
	# The install name is what the test binaries load at run time, so it has
	# to be absolute; `make clean` after moving the checkout.
	cc -dynamiclib -install_name "$SHIM" -o "$SHIM" "$SHIM_SRC"
fi
provided=$(nm -gU "$SHIM" | awk '{ print $3 }' | sort)

status=0
for test in "${TESTS[@]}"; do
	# `cargo rustc` reaches only the final link of this one test target, so
	# the dependency graph stays shared with an ordinary `cargo test`.
	bin=$(cargo rustc -q -p irl-source --test "$test" --message-format=json \
		-- -C link-arg=-Wl,-needed_library,"$SHIM" \
		| grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
	if [[ -z "$bin" ]]; then
		echo "$test: cargo rustc reported no executable" >&2
		exit 1
	fi
	# Every libobs import must be one the shim defines, or the run would fault
	# on a null pointer with no symbol name attached.
	imports=$(nm -u "$bin" \
		| grep -E '^_(blog|obs_|os_|bmalloc|bzalloc|brealloc|bfree|bstrdup|calldata_|video_|audio_|proc_handler_|signal_handler_)' \
		| sort || true)
	missing=$(comm -23 <(echo "$imports") <(echo "$provided") | sed '/^$/d')
	if [[ -n "$missing" ]]; then
		echo "$test imports libobs symbols the shim does not define:" >&2
		echo "$missing" | sed 's/^/  /' >&2
		echo "add them to $SHIM_SRC or drop the test from $0" >&2
		exit 1
	fi
	echo "== $test"
	"$bin" "$@" || status=1
done
exit $status
