#!/usr/bin/env bash
#
# obs-irl-source — post-link checks for a bundled-stack build (Linux, macOS).
#
# Copyright (C) 2026 Thomas Lekanger
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# The whole point of bundling FFmpeg is that the plugin stops depending on the
# host OBS's copy, and that its own copy stays invisible to the rest of the
# process. Both properties are invisible in a successful compile and easy to
# lose to a stray link flag, so they get asserted here instead.
#
# Usage: scripts/verify-plugin.sh target/release/libobs_irl_source.so

set -euo pipefail

module="${1:?usage: verify-plugin.sh <path-to-plugin>}"
[[ -f ${module} ]] || {
	echo "no such file: ${module}" >&2
	exit 1
}

fail=0
check() {
	if [[ $1 -eq 0 ]]; then
		printf '  ok    %s\n' "$2"
	else
		printf '  FAIL  %s\n' "$2"
		fail=1
	fi
}

echo "verifying ${module}"

# Source-level, checked on every platform: the plugin crates must stay free of
# unsafe code. Every raw pointer belongs in crates/obs-sys, crates/obs and
# crates/ffmpeg; the compiler enforces #![forbid(unsafe_code)] where present,
# this only catches the attribute being deleted.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for f in crates/irl-core/src/lib.rs crates/irl-source/src/lib.rs; do
	if [[ -f ${repo_root}/${f} ]]; then
		grep -q '^#!\[forbid(unsafe_code)\]' "${repo_root}/${f}" && r=0 || r=1
		check ${r} "forbid(unsafe_code) present in ${f}"
	fi
done

case "$(uname -s)" in
Linux*)
	needed="$(readelf -d "${module}" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')"
	exports="$(nm -D --defined-only "${module}" | awk '$2 != "A" && $2 != "a" {print $3}')"

	echo "${needed}" | grep -qE '^lib(av|sw)' && r=1 || r=0
	check ${r} "no libav*/libsw* in DT_NEEDED (host FFmpeg not required)"

	echo "${needed}" | grep -q '^libva\.so' && r=0 || r=1
	check ${r} "libva present (VAAPI hardware decode compiled in)"

	echo "${exports}" | grep -vqE '^obs_module_' && r=1 || r=0
	check ${r} "exports limited to obs_module_*"

	echo "${exports}" | grep -q '^obs_module_load$' && r=0 || r=1
	check ${r} "obs_module_load exported"

	# libobs is never linked: its symbols stay undefined and resolve
	# against the host process at dlopen time. Anything else undefined
	# means a static dependency was dropped from the link line.
	undef="$(nm -D --undefined-only "${module}" | awk '{print $NF}' | sed 's/@.*//')"
	echo "${undef}" | grep -vqE '^(obs_[a-z_0-9]+|os_gettime_ns|os_sleep_ms|blog|bfree|calldata_[a-z_]+|proc_handler_[a-z_]+|text_lookup_[a-z_]+|video_format_get_parameters_for_format|__[a-z_A-Z0-9]+|_[A-Z][a-zA-Z_0-9]*|[a-z_0-9]+)$' && r=1 || r=0
	# The last alternative accepts libc/libm/libva/libstdc++ symbols by
	# shape; the real guard is DT_NEEDED above plus the loader.
	check ${r} "undefined symbols are libobs or system libraries"
	;;

Darwin*)
	needed="$(otool -L "${module}" | tail -n +2 | awk '{print $1}')"
	# -j prints just the symbol name. Column parsing is not viable here:
	# nm renders an indirect symbol with an empty address column, which
	# shifts every field and turns "(indirect" into an apparent export.
	exports="$(nm -gUj "${module}" | grep -v '^$')"

	echo "${needed}" | grep -qE '/lib(av|sw)[a-z]*\.' && r=1 || r=0
	check ${r} "no libav*/libsw* in load commands (host FFmpeg not required)"

	# Everything must resolve on a stock Mac: @rpath for libobs, /usr/lib
	# and /System for the OS. An absolute path anywhere else (a Homebrew
	# prefix, say) is a dependency the user does not have.
	echo "${needed}" | grep -qvE '^(@rpath/|/usr/lib/|/System/Library/)' && r=1 || r=0
	check ${r} "no non-system absolute paths in load commands"

	echo "${exports}" | grep -vqE '^_obs_module_' && r=1 || r=0
	check ${r} "exports limited to _obs_module_*"

	echo "${exports}" | grep -q '^_obs_module_load$' && r=0 || r=1
	check ${r} "_obs_module_load exported"
	;;

*)
	echo "unsupported platform for this script (Windows uses dumpbin in CI)" >&2
	exit 1
	;;
esac

echo
if [[ ${fail} -ne 0 ]]; then
	echo "verification FAILED" >&2
	echo
	echo "dependencies:"
	echo "${needed}" | sed 's/^/  /'
	echo "exports:"
	echo "${exports}" | sed 's/^/  /'
	exit 1
fi
echo "verification passed"
