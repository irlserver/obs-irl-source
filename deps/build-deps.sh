#!/usr/bin/env bash
#
# obs-irl-source — build the bundled media stack
# (mbedTLS, libsrt, librist, nv-codec-headers, FFmpeg).
#
# Copyright (C) 2026 Thomas Lekanger
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Produces static libraries plus a generated irl-deps.cmake describing the
# exact link line. On Windows this runs inside an MSYS2 bash with the MSVC
# environment already active (FFmpeg's configure needs a POSIX shell even when
# it drives cl.exe).
#
# Usage:
#   deps/build-deps.sh [--prefix DIR] [--jobs N] [--clean]

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "${script_dir}/.." && pwd)"

# shellcheck source=versions.env
source "${script_dir}/versions.env"

prefix="${IRL_DEPS_PREFIX:-${repo_dir}/deps/.build/prefix}"
work="${IRL_DEPS_WORK:-${repo_dir}/deps/.build}"
jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
clean=0

while [[ $# -gt 0 ]]; do
	case "$1" in
	--prefix)
		prefix="$2"
		shift 2
		;;
	--jobs)
		jobs="$2"
		shift 2
		;;
	--clean)
		clean=1
		shift
		;;
	-h | --help)
		sed -n '2,20p' "${BASH_SOURCE[0]}"
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		exit 1
		;;
	esac
done

case "$(uname -s)" in
Linux*) host=linux ;;
Darwin*) host=macos ;;
MINGW* | MSYS* | CYGWIN*) host=windows ;;
*)
	echo "unsupported host: $(uname -s)" >&2
	exit 1
	;;
esac

if [[ ${host} == windows ]]; then
	# MSYS2 ships a coreutils /usr/bin/link.exe that shadows MSVC's linker.
	# CMake is unaffected (it resolves the linker next to the compiler), but
	# meson probes PATH and aborts with "Found GNU link.exe instead of MSVC
	# link.exe". cl.exe and link.exe live in the same directory, so putting
	# that directory first fixes it without modifying the MSYS2 install.
	if ! command -v cl >/dev/null 2>&1; then
		echo "cl.exe is not on PATH. Run this from an MSVC environment" >&2
		echo "(the CI job uses ilammy/msvc-dev-cmd plus msys2 path-type: inherit)." >&2
		exit 1
	fi
	PATH="$(dirname "$(command -v cl)"):${PATH}"
	export PATH
fi

downloads="${work}/downloads"
src="${work}/src"

if [[ ${clean} -eq 1 ]]; then
	rm -rf "${src}" "${prefix}"
fi
mkdir -p "${downloads}" "${src}" "${prefix}"

# Absolute, forward-slash prefix. MSYS2 hands cl.exe/cmake native paths, so
# translate once here rather than at every use site.
prefix="$(cd "${prefix}" && pwd)"
if [[ ${host} == windows ]]; then
	native_prefix="$(cygpath -m "${prefix}")"
else
	native_prefix="${prefix}"
fi

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# Path in the form the tool expects. FFmpeg's build is MSYS-native and takes
# MSYS paths, but cmake.exe and cl.exe are Windows binaries that cannot read
# them, so anything handed to CMake goes through here first.
npath() {
	if [[ ${host} == windows ]]; then
		cygpath -m "$1"
	else
		printf '%s' "$1"
	fi
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	else
		shasum -a 256 "$1" | cut -d' ' -f1
	fi
}

# fetch <url> <filename> <sha256>
fetch() {
	local url="$1" file="$2" want="$3" path="${downloads}/$2"

	if [[ -f ${path} ]] && [[ "$(sha256_of "${path}")" == "${want}" ]]; then
		echo "cached: ${file}"
		return
	fi

	echo "download: ${url}"
	# --retry alone only covers timeouts and 5xx; a refused or reset
	# connection is not "transient" to curl and fails on the first try.
	# ffmpeg.org does both often enough to have cost a CI run.
	curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors \
		--retry-connrefused --connect-timeout 30 \
		-o "${path}.tmp" "${url}"

	local got
	got="$(sha256_of "${path}.tmp")"
	if [[ ${got} != "${want}" ]]; then
		rm -f "${path}.tmp"
		echo "checksum mismatch for ${file}: got ${got}, expected ${want}" >&2
		exit 1
	fi
	mv "${path}.tmp" "${path}"
}

# extract <filename> <dest-dir-name>
extract() {
	local file="$1" dest="${src}/$2"
	[[ -d ${dest} ]] && return
	mkdir -p "${dest}"
	tar -xf "${downloads}/${file}" -C "${dest}" --strip-components=1
}

# Marker files keep an interrupted or repeated run from redoing finished work.
# They live inside the prefix so CI can cache the prefix alone: restoring it
# without the (much larger) source and object trees still reads as "built".
built() { [[ -f "${prefix}/.built-$1-$2" ]]; }
mark_built() { touch "${prefix}/.built-$1-$2"; }

export PKG_CONFIG_PATH="${prefix}/lib/pkgconfig:${prefix}/lib64/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"

# Both libsrt and librist record their mbedTLS dependency in their .pc file as
# an absolute library path. That breaks a static link twice over:
#
#   1. FFmpeg's configure treats anything that is not -lfoo as a compiler flag
#      and emits it *before* the -lsrt / -lrist it belongs to, which a
#      single-pass linker cannot resolve.
#   2. Whichever mbedTLS the dependency's build system happened to find gets
#      baked in by path. librist's meson picked up the host's shared
#      libmbedcrypto.so, which would both reintroduce a runtime dependency the
#      bundling exists to remove and mismatch the copy libsrt uses.
#
# Rewriting the reference to plain -l flags, appended after the library's own
# entry, fixes the ordering and pins it to the mbedTLS in this prefix.
# FFmpeg's msvc toolchain rewrites the -lfoo it reads from a .pc file into
# foo.lib, but neither CMake nor meson necessarily names its static output that
# way: libsrt builds srt_static.lib, meson builds librist.a. The mismatch shows
# up as "X not found using pkg-config" from a library that just installed
# successfully. Provide the name the linker will ask for.
#
# Patching the .pc to an absolute path would also resolve it, and would also
# reintroduce the link-ordering trap fix_mbedtls_pc exists to undo.
# ensure_msvc_lib_name <-l name> [extra basename ...]
ensure_msvc_lib_name() {
	local want="$1"
	shift
	[[ ${host} == windows ]] || return 0

	local dst="${prefix}/lib/${want}.lib"
	[[ -f ${dst} ]] && return 0

	local cand base
	local candidates=(
		"${want}_static"
		"lib${want}"
		"$@"
	)
	for base in "${candidates[@]}"; do
		for cand in "${prefix}/lib/${base}.lib" "${prefix}/lib/${base}.a"; do
			if [[ -f ${cand} ]]; then
				cp "${cand}" "${dst}"
				echo "provided $(basename "${dst}") from $(basename "${cand}")"
				return 0
			fi
		done
	done

	echo "no static library found for -l${want} in ${prefix}/lib" >&2
	ls -1 "${prefix}/lib" >&2 || true
	exit 1
}

fix_mbedtls_pc() {
	local pc="$1" field="$2"
	[[ -f ${pc} ]] || return 0
	sed -i.bak -E \
		-e 's![^[:space:]]*/lib(mbedtls|mbedx509|mbedcrypto|everest|p256m)\.(a|so[0-9.]*|dylib|lib)!!g' \
		-e 's!-l(mbedtls|mbedx509|mbedcrypto)([[:space:]]|$)!\2!g' \
		-e "s!^(${field}:.*)\$!\\1 -L\\\${libdir} -lmbedtls -lmbedx509 -lmbedcrypto!" \
		"${pc}"
	rm -f "${pc}.bak"
}

cmake_common=(
	-DCMAKE_BUILD_TYPE=Release
	-DCMAKE_INSTALL_PREFIX="${native_prefix}"
	-DCMAKE_PREFIX_PATH="${native_prefix}"
	-DCMAKE_POSITION_INDEPENDENT_CODE=ON
	-DBUILD_SHARED_LIBS=OFF
)
if [[ ${host} == macos ]]; then
	cmake_common+=(-DCMAKE_OSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}")
fi
if [[ ${host} == windows ]]; then
	# Ninja avoids the MSBuild/MSYS path translation mess entirely.
	cmake_common+=(-G Ninja -DCMAKE_C_COMPILER=cl -DCMAKE_CXX_COMPILER=cl)
	# Pin the dynamic CRT everywhere. See IRL_MSVC_CRT below; CMP0091 has to
	# be forced because a dependency declaring cmake_minimum_required below
	# 3.15 gets the old policy and silently ignores the runtime setting.
	cmake_common+=(
		-DCMAKE_POLICY_DEFAULT_CMP0091=NEW
		-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL
	)
fi

# Everything that ends up in one DLL must agree on the C runtime. Mixing them
# is not merely a link error: /MD and /MT binaries get separate heaps and
# separate stdio state, so a buffer allocated in one and freed in the other
# corrupts the process.
#
# cl.exe defaults to /MT when given no flag, while CMake and meson both emit
# /MD. FFmpeg's configure passes nothing, so it silently built /MT against /MD
# dependencies and failed with unresolved __imp_* CRT imports. The plugin DLL
# itself is /MD (CMake's default, and what libobs uses), so /MD is the target
# and all three build systems are pinned to it explicitly rather than left to
# their defaults.
#
# Seeded with the options every platform needs, so the array is never empty:
# macOS still ships bash 3.2, where expanding an empty array under `set -u` is
# an unbound-variable error.
meson_common=(--buildtype=release --default-library=static)
if [[ ${host} == windows ]]; then
	meson_common+=(-Db_vscrt=md)
fi

# ── zlib ─────────────────────────────────────────────────────────────────────
# Windows only. Linux and macOS supply zlib as a system library, which the
# builds there already link. Building it here rather than dropping
# --enable-zlib on Windows keeps the FFmpeg feature set identical on all three
# platforms; a capability that silently differs per platform is worse than a
# short extra build step.
build_zlib() {
	[[ ${host} == windows ]] || return 0
	built zlib "${ZLIB_VERSION}" && return
	log "zlib ${ZLIB_VERSION}"

	fetch "https://github.com/madler/zlib/releases/download/v${ZLIB_VERSION}/zlib-${ZLIB_VERSION}.tar.gz" \
		"zlib-${ZLIB_VERSION}.tar.gz" "${ZLIB_SHA256}"
	extract "zlib-${ZLIB_VERSION}.tar.gz" "zlib-${ZLIB_VERSION}"

	# ZLIB_BUILD_TESTING is what 1.3.2 renamed ZLIB_BUILD_EXAMPLES to; both
	# are passed so either version builds only the library.
	#
	# ZLIB_BUILD_SHARED/STATIC are zlib's own switches and both default ON;
	# it does not honour BUILD_SHARED_LIBS from cmake_common. Left alone it
	# installs z.dll plus an *import* z.lib alongside the static library,
	# and since that import lib already occupies the name FFmpeg links
	# against, ensure_msvc_lib_name below would accept it and quietly give
	# the plugin a runtime DLL dependency the bundled stack exists to avoid.
	cmake -S "$(npath "${src}/zlib-${ZLIB_VERSION}")" \
		-B "$(npath "${src}/zlib-${ZLIB_VERSION}/build")" \
		"${cmake_common[@]}" \
		-DZLIB_BUILD_SHARED=OFF \
		-DZLIB_BUILD_STATIC=ON \
		-DZLIB_BUILD_TESTING=OFF \
		-DZLIB_BUILD_EXAMPLES=OFF
	cmake --build "$(npath "${src}/zlib-${ZLIB_VERSION}/build")" --parallel "${jobs}"
	cmake --install "$(npath "${src}/zlib-${ZLIB_VERSION}/build")"

	# Belt and braces: if a future zlib renames those switches the way 1.3.2
	# renamed its static target, fail here rather than link a DLL.
	if [[ -f ${prefix}/bin/z.dll || -f ${prefix}/bin/zlib1.dll ]]; then
		echo "zlib installed a DLL; the bundled stack must be static" >&2
		exit 1
	fi

	# FFmpeg's MSVC flag translator hardcodes -lz to zlib.lib rather than
	# z.lib like every other -l name:
	#
	#     -lz)   echo zlib.lib ;;
	#     -l*)   echo ${flag#-l}.lib ;;
	#
	# Under zlib 1.3.1 that name existed by accident, as the *shared*
	# import library (1.3.1 named the DLL target zlib; 1.3.2 renamed it to
	# z). So the Windows build has been linking zlib dynamically all along,
	# and turning the DLL off is what finally made the missing name visible
	# as LNK1181: cannot open input file 'zlib.lib'.
	#
	# Provide both spellings from the static archive: zlib.lib is what
	# FFmpeg links, z.lib is what the generic -l handling in the CMake
	# description below resolves.
	ensure_msvc_lib_name z zs zlibstatic zlib
	ensure_msvc_lib_name zlib zs zlibstatic z

	# zconf.h.cmakein still carries an autoconf-era block:
	#
	#     #ifdef HAVE_UNISTD_H
	#     #  define Z_HAVE_UNISTD_H
	#     #endif
	#
	# CMake probes for unistd.h, does not find it under MSVC, and correctly
	# leaves Z_HAVE_UNISTD_H undefined earlier in the file. That block then
	# defines it anyway, because FFmpeg's config.h contains
	# "#define HAVE_UNISTD_H 0" and #ifdef is true for a value of zero. The
	# header goes on to include <unistd.h>, which MSVC does not have, and
	# every FFmpeg source that touches zlib fails to compile.
	#
	# zlib's own ./configure rewrites this line for exactly this reason. The
	# CMake path does not, so do it here.
	local zconf="${prefix}/include/zconf.h"
	if [[ -f ${zconf} ]]; then
		sed -i.bak \
			's!^#ifdef HAVE_UNISTD_H.*!#if 0 /* patched: MSVC has no unistd.h, and FFmpeg defines HAVE_UNISTD_H to 0 */!' \
			"${zconf}"
		rm -f "${zconf}.bak"
		if grep -q '^#ifdef HAVE_UNISTD_H' "${zconf}"; then
			echo "failed to patch HAVE_UNISTD_H out of ${zconf}" >&2
			exit 1
		fi
	fi

	mark_built zlib "${ZLIB_VERSION}"
}

# ── mbedTLS ──────────────────────────────────────────────────────────────────
# Supplies TLS for FFmpeg (https, rtmps) and AES for libsrt passphrases.
# Apache-2.0, which is compatible with the LGPLv3 FFmpeg build below.
build_mbedtls() {
	built mbedtls "${MBEDTLS_VERSION}" && return
	log "mbedTLS ${MBEDTLS_VERSION}"

	fetch "https://github.com/Mbed-TLS/mbedtls/releases/download/mbedtls-${MBEDTLS_VERSION}/mbedtls-${MBEDTLS_VERSION}.tar.bz2" \
		"mbedtls-${MBEDTLS_VERSION}.tar.bz2" "${MBEDTLS_SHA256}"
	extract "mbedtls-${MBEDTLS_VERSION}.tar.bz2" "mbedtls-${MBEDTLS_VERSION}"

	cmake -S "$(npath "${src}/mbedtls-${MBEDTLS_VERSION}")" \
		-B "$(npath "${src}/mbedtls-${MBEDTLS_VERSION}/build")" \
		"${cmake_common[@]}" \
		-DENABLE_TESTING=OFF \
		-DENABLE_PROGRAMS=OFF \
		-DUSE_STATIC_MBEDTLS_LIBRARY=ON \
		-DUSE_SHARED_MBEDTLS_LIBRARY=OFF \
		-DMBEDTLS_AS_SUBPROJECT=OFF
	cmake --build "$(npath "${src}/mbedtls-${MBEDTLS_VERSION}/build")" --parallel "${jobs}"
	cmake --install "$(npath "${src}/mbedtls-${MBEDTLS_VERSION}/build")"

	# mbedTLS draws entropy from BCryptGenRandom on Windows but its generated
	# .pc files do not declare bcrypt, so anything linking it statically via
	# pkg-config fails on that one symbol. FFmpeg reports it as the wholly
	# misleading "ERROR: mbedTLS not found".
	if [[ ${host} == windows ]]; then
		local pc
		for pc in "${prefix}/lib/pkgconfig/mbedcrypto.pc" \
			"${prefix}/lib/pkgconfig/mbedtls.pc"; do
			[[ -f ${pc} ]] || continue
			grep -q -- '-lbcrypt' "${pc}" && continue
			sed -i.bak -E 's!^(Libs:.*)$!\1 -lbcrypt!' "${pc}"
			rm -f "${pc}.bak"
		done
	fi

	mark_built mbedtls "${MBEDTLS_VERSION}"
}

# ── libsrt ───────────────────────────────────────────────────────────────────
build_srt() {
	built srt "${SRT_VERSION}" && return
	log "libsrt ${SRT_VERSION}"

	fetch "https://github.com/Haivision/srt/archive/refs/tags/v${SRT_VERSION}.tar.gz" \
		"srt-${SRT_VERSION}.tar.gz" "${SRT_SHA256}"
	extract "srt-${SRT_VERSION}.tar.gz" "srt-${SRT_VERSION}"

	cmake -S "$(npath "${src}/srt-${SRT_VERSION}")" \
		-B "$(npath "${src}/srt-${SRT_VERSION}/build")" \
		"${cmake_common[@]}" \
		-DENABLE_SHARED=OFF \
		-DENABLE_STATIC=ON \
		-DENABLE_APPS=OFF \
		-DENABLE_EXAMPLES=OFF \
		-DENABLE_UNITTESTS=OFF \
		-DENABLE_ENCRYPTION=ON \
		-DUSE_ENCLIB=mbedtls \
		-DENABLE_CXX11=ON
	cmake --build "$(npath "${src}/srt-${SRT_VERSION}/build")" --parallel "${jobs}"
	cmake --install "$(npath "${src}/srt-${SRT_VERSION}/build")"

	fix_mbedtls_pc "${prefix}/lib/pkgconfig/srt.pc" "Libs.private"
	ensure_msvc_lib_name srt

	mark_built srt "${SRT_VERSION}"
}

# ── librist ──────────────────────────────────────────────────────────────────
# Meson, not CMake, which is why the deps build needs meson/ninja everywhere.
# lz4 and cJSON use librist's vendored copies so this pulls in no further
# system dependencies; mbedTLS is the one we already built, giving RIST its
# encryption support.
build_librist() {
	built librist "${LIBRIST_VERSION}" && return
	log "librist ${LIBRIST_VERSION}"

	fetch "https://code.videolan.org/rist/librist/-/archive/v${LIBRIST_VERSION}/librist-v${LIBRIST_VERSION}.tar.gz" \
		"librist-${LIBRIST_VERSION}.tar.gz" "${LIBRIST_SHA256}"
	extract "librist-${LIBRIST_VERSION}.tar.gz" "librist-${LIBRIST_VERSION}"

	local rist="${src}/librist-${LIBRIST_VERSION}"
	rm -rf "${rist}/build"
	(
		cd "${rist}"
		# librist locates mbedTLS with cc.find_library, which searches
		# the default system paths and would happily bind the host's
		# shared libmbedcrypto. Point the compiler at this prefix first
		# so it finds the static copy libsrt is already using.
		meson setup build \
			"${meson_common[@]}" \
			--prefix="${native_prefix}" \
			-Dc_args="-I${native_prefix}/include" \
			-Dc_link_args="-L${native_prefix}/lib" \
			-Dbuilt_tools=false \
			-Dtest=false \
			-Dbuiltin_lz4=true \
			-Dbuiltin_cjson=true \
			-Dbuiltin_mbedtls=false \
			-Duse_mbedtls=true
		meson compile -C build
		meson install -C build
	)

	fix_mbedtls_pc "${prefix}/lib/pkgconfig/librist.pc" "Libs"

	ensure_msvc_lib_name rist

	mark_built librist "${LIBRIST_VERSION}"
}

# ── nv-codec-headers ─────────────────────────────────────────────────────────
build_nvcodec() {
	[[ ${host} == macos ]] && return 0
	built nvcodec "${NVCODEC_VERSION}" && return
	log "nv-codec-headers ${NVCODEC_VERSION}"

	fetch "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/n${NVCODEC_VERSION}.tar.gz" \
		"nv-codec-headers-${NVCODEC_VERSION}.tar.gz" "${NVCODEC_SHA256}"
	extract "nv-codec-headers-${NVCODEC_VERSION}.tar.gz" "nv-codec-headers-${NVCODEC_VERSION}"

	make -C "${src}/nv-codec-headers-${NVCODEC_VERSION}" install PREFIX="${prefix}"

	mark_built nvcodec "${NVCODEC_VERSION}"
}

# ── FFmpeg ───────────────────────────────────────────────────────────────────
#
# Decode-only and deliberately narrow: --disable-everything, then only the
# components an IRL ingest actually touches. Keeping the surface small is what
# makes a static link viable size-wise.
#
# LGPLv3 (--enable-version3, no --enable-gpl). Nothing here needs GPL
# components: the plugin decodes, it never encodes.

FFMPEG_DECODERS="h264,hevc,av1,vp9,aac,aac_latm,aac_fixed,opus,mp3,mp3float,ac3,ac3_fixed,eac3,pcm_s16le,pcm_s16be,pcm_s24le,pcm_s32le,pcm_u8,pcm_f32le,pcm_alaw,pcm_mulaw"
FFMPEG_PARSERS="h264,hevc,av1,vp9,aac,aac_latm,ac3,mpegaudio,opus"
FFMPEG_DEMUXERS="mpegts,mpegtsraw,flv,live_flv,mov,matroska,hls,rtsp,sdp,rtp,h264,hevc,av1,ivf,aac,mp3,ac3,wav,mpjpeg,data"
# SRT and RIST are "libsrt"/"librist" — FFmpeg names each protocol after the
# library. There is no "hls" protocol either; HLS is a demuxer that drives http.
FFMPEG_PROTOCOLS="file,pipe,data,cache,concat,tcp,udp,rtp,http,https,httpproxy,tls,crypto,libsrt,librist,rtmp,rtmpe,rtmps,rtmpt,rtmpte,rtmpts"
FFMPEG_BSFS="h264_mp4toannexb,hevc_mp4toannexb,av1_frame_merge,vp9_superframe,vp9_superframe_split,extract_extradata,aac_adtstoasc,null"

# Components whose absence would silently degrade the plugin rather than fail
# the build. FFmpeg's configure ignores unknown names in an --enable-* list, so
# a typo here disables a feature without any diagnostic: --enable-protocol=srt
# happily produced a build with CONFIG_LIBSRT=yes and no SRT protocol at all.
FFMPEG_REQUIRED_CONFIG="
LIBSRT_PROTOCOL
LIBRIST_PROTOCOL
MBEDTLS
ZLIB
TLS_PROTOCOL
RTMP_PROTOCOL
HTTPS_PROTOCOL
MPEGTS_DEMUXER
FLV_DEMUXER
MATROSKA_DEMUXER
H264_DECODER
HEVC_DECODER
AV1_DECODER
VP9_DECODER
AAC_DECODER
OPUS_DECODER
MP3_DECODER
AC3_DECODER
SWSCALE
SWRESAMPLE
"

build_ffmpeg() {
	built ffmpeg "${FFMPEG_VERSION}" && return
	log "FFmpeg ${FFMPEG_VERSION}"

	fetch "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
		"ffmpeg-${FFMPEG_VERSION}.tar.xz" "${FFMPEG_SHA256}"
	extract "ffmpeg-${FFMPEG_VERSION}.tar.xz" "ffmpeg-${FFMPEG_VERSION}"

	local ff="${src}/ffmpeg-${FFMPEG_VERSION}"

	# Upstream bug. libavformat/tls_mbedtls.c calls gmtime_r without
	# including libavutil/time_internal.h, which is where FFmpeg keeps the
	# ff_gmtime_r fallback for platforms that lack the POSIX function.
	# Everywhere except MSVC the real gmtime_r resolves and the missing
	# include is invisible, so it surfaces only as a lone unresolved
	# external when the plugin links.
	#
	# Applied on every platform: the include is a no-op where HAVE_GMTIME_R
	# is set, and keeping one source state across platforms beats carrying a
	# Windows-only divergence.
	local tls="${ff}/libavformat/tls_mbedtls.c"
	if [[ -f ${tls} ]] && ! grep -q 'libavutil/time_internal.h' "${tls}"; then
		sed -i.bak \
			's!^#include "libavutil/random_seed.h"!&\n#include "libavutil/time_internal.h"!' \
			"${tls}"
		rm -f "${tls}.bak"
		if ! grep -q 'libavutil/time_internal.h' "${tls}"; then
			echo "failed to patch gmtime_r include into ${tls}" >&2
			echo "check whether the anchor include still exists upstream." >&2
			exit 1
		fi
		echo "patched gmtime_r include into libavformat/tls_mbedtls.c"
	fi

	# Upstream performance bug, Windows-only code. The D3D11VA download
	# (d3d11va_transfer_data) reads the mapped staging texture with
	# av_image_copy2 — plain cached loads. Drivers may place a READ|WRITE
	# staging texture in write-combined memory, where cached loads run at
	# a small fraction of memory speed; one 4K60 stream (~716MB/s of
	# NV12) then pins an entire core inside the copy, and the whole thing
	# runs under the shared D3D11 device lock the decoder also submits
	# through. The DXVA2 path has used av_image_copy_uc_from — SSE4
	# streaming loads written for exactly this kind of memory — since
	# 2017; D3D11VA never got the same treatment. Swap the download copy
	# over to it. Safe by construction: av_image_copy_uc_from falls back
	# to the plain copy on its own when SSE4 or the 64-byte alignment it
	# wants is missing, so the worst case is today's behaviour.
	if [[ ${host} == windows ]]; then
		local d3d="${ff}/libavutil/hwcontext_d3d11va.c"
		if [[ -f ${d3d} ]] && ! grep -q 'av_image_copy_uc_from' "${d3d}"; then
			sed -i '/av_image_copy2(dst->data, dst->linesize, map_data, map_linesize,/{N;s|av_image_copy2(dst->data, dst->linesize, map_data, map_linesize,\n *ctx->sw_format, w, h);|{\n            ptrdiff_t uc_dst_ls[4], uc_src_ls[4];\n            for (int uc_k = 0; uc_k < 4; uc_k++) {\n                uc_dst_ls[uc_k] = dst->linesize[uc_k];\n                uc_src_ls[uc_k] = map_linesize[uc_k];\n            }\n            av_image_copy_uc_from(dst->data, uc_dst_ls,\n                                  (const uint8_t **)map_data, uc_src_ls,\n                                  ctx->sw_format, w, h);\n        }|}' "${d3d}"
			if ! grep -q 'av_image_copy_uc_from' "${d3d}"; then
				echo "failed to patch the D3D11VA download copy in ${d3d}" >&2
				echo "check whether d3d11va_transfer_data still uses av_image_copy2 upstream." >&2
				exit 1
			fi
			echo "patched D3D11VA download to av_image_copy_uc_from"
		fi
	fi

	# VAAPI vaMapBuffer2: FFmpeg 9.0 calls vaMapBuffer2 when the libva headers
	# report VA 1.21+ (libva 2.21+) and plain vaMapBuffer below that. The Linux
	# artifact is built on ubuntu-22.04 for glibc 2.35 (Flatpak, #29) and 22.04
	# ships libva 2.14, so a stock build there loses the read/write mapping hint
	# everywhere it runs — including inside the Flatpak runtime, whose libva does
	# have vaMapBuffer2. libva is linked dynamically while this FFmpeg is static,
	# so the call can be recovered at runtime: probe vaMapBuffer2 with dlsym and
	# fall back to vaMapBuffer when it is absent. One binary, fast path wherever
	# the loaded libva offers it.
	#
	# extract() leaves an existing source tree alone, so the file is restored
	# from the tarball first: patching is then always a pristine-in, patched-out
	# transform and needs no idempotence of its own.
	if [[ ${host} == linux ]]; then
		local vaapi_rel="libavutil/hwcontext_vaapi.c"
		if [[ -f ${ff}/${vaapi_rel} ]]; then
			tar -xf "${downloads}/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
				-C "${ff}" --strip-components=1 \
				"ffmpeg-${FFMPEG_VERSION}/${vaapi_rel}"
			python3 - "${ff}/${vaapi_rel}" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text()

# `uint32_t vaflags` is declared under the same version guard as the call.
# Make it unconditional so the runtime probe can always use it.
decl_old = """#if VA_CHECK_VERSION(1, 21, 0)
    uint32_t vaflags = 0;
#endif
"""
decl_new = """    uint32_t vaflags = 0;
"""

call_old = """#if VA_CHECK_VERSION(1, 21, 0)
    if (flags & AV_HWFRAME_MAP_READ)
        vaflags |= VA_MAPBUFFER_FLAG_READ;
    if (flags & AV_HWFRAME_MAP_WRITE)
        vaflags |= VA_MAPBUFFER_FLAG_WRITE;
    // On drivers not implementing vaMapBuffer2 libva calls vaMapBuffer instead.
    vas = vaMapBuffer2(hwctx->display, map->image.buf, &address, vaflags);
#else
    vas = vaMapBuffer(hwctx->display, map->image.buf, &address);
#endif
"""
# Values match libva's va.h; redefined only so the build headers need not
# declare them. The signature is libva's: the flags argument is uint32_t.
call_new = """    /* IRL_VAAPI_RUNTIME_WEAK: libva is dynamic while this FFmpeg is static,
     * so vaMapBuffer2 is recoverable at runtime even when the build headers
     * (libva 2.14 on ubuntu-22.04) do not declare it. */
#ifndef VA_MAPBUFFER_FLAG_READ
#define VA_MAPBUFFER_FLAG_READ  1
#endif
#ifndef VA_MAPBUFFER_FLAG_WRITE
#define VA_MAPBUFFER_FLAG_WRITE 2
#endif
    {
        typedef VAStatus (*irl_map_buffer2)(VADisplay, VABufferID, void **, uint32_t);
        static irl_map_buffer2 map2;
        static int map2_probed;

        if (!map2_probed) {
            map2_probed = 1;
            map2 = (irl_map_buffer2)dlsym(RTLD_DEFAULT, "vaMapBuffer2");
        }

        if (map2) {
            if (flags & AV_HWFRAME_MAP_READ)
                vaflags |= VA_MAPBUFFER_FLAG_READ;
            if (flags & AV_HWFRAME_MAP_WRITE)
                vaflags |= VA_MAPBUFFER_FLAG_WRITE;
            vas = map2(hwctx->display, map->image.buf, &address, vaflags);
        } else {
            vas = vaMapBuffer(hwctx->display, map->image.buf, &address);
        }
    }
"""

for name, old in (("vaflags declaration", decl_old), ("vaMapBuffer2 call", call_old)):
    if text.count(old) != 1:
        print(f"VAAPI patch: {name} anchor not found exactly once; check whether "
              f"it still looks this way upstream", file=sys.stderr)
        sys.exit(1)

text = text.replace(decl_old, decl_new).replace(call_old, call_new)

# RTLD_DEFAULT is a GNU extension, so _GNU_SOURCE must be defined before any
# system header — hence the top of the file, ahead of dlfcn.h.
text = ("""#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <dlfcn.h>
""" + text)

path.write_text(text)
print("patched VAAPI to runtime-weak vaMapBuffer2")
PY
			if ! grep -q 'IRL_VAAPI_RUNTIME_WEAK' "${ff}/${vaapi_rel}"; then
				echo "failed to patch runtime-weak vaMapBuffer2 into ${ff}/${vaapi_rel}" >&2
				exit 1
			fi
		fi
	fi

	local args=(
		--prefix="${prefix}"
		--disable-shared
		--enable-static
		--enable-pic
		--enable-version3
		--disable-programs
		--disable-doc
		--disable-avdevice
		--disable-avfilter
		--disable-everything
		--disable-autodetect
		--enable-swscale
		--enable-swresample
		--enable-network
		--enable-zlib
		--enable-mbedtls
		--enable-libsrt
		--enable-librist
		--enable-decoder="${FFMPEG_DECODERS}"
		--enable-parser="${FFMPEG_PARSERS}"
		--enable-demuxer="${FFMPEG_DEMUXERS}"
		--enable-protocol="${FFMPEG_PROTOCOLS}"
		--enable-bsf="${FFMPEG_BSFS}"
		--pkg-config-flags=--static
	)

	case "${host}" in
	linux)
		args+=(
			--enable-ffnvcodec
			--enable-cuda
			--enable-nvdec
		)
		# VAAPI links libva at build time, so a plugin built with it
		# carries a hard DT_NEEDED on libva.so.2. That is the accepted
		# trade for Intel/AMD hardware decode, but it must never be
		# dropped silently: a release build without VAAPI would ship
		# software-only decode with no visible failure.
		if [[ ${IRL_DEPS_DISABLE_VAAPI:-0} == 1 ]]; then
			echo "warning: VAAPI disabled by IRL_DEPS_DISABLE_VAAPI — do not ship this build" >&2
			args+=(--enable-hwaccel=h264_nvdec,hevc_nvdec,av1_nvdec,vp9_nvdec)
		elif pkg-config --exists libva libva-drm; then
			args+=(
				--enable-vaapi
				--enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi,vp9_vaapi,h264_nvdec,hevc_nvdec,av1_nvdec,vp9_nvdec
			)
		else
			echo "libva development files not found (install libva-dev)." >&2
			echo "Set IRL_DEPS_DISABLE_VAAPI=1 to build without hardware decode on Intel/AMD." >&2
			exit 1
		fi
		# Runtime-weak vaMapBuffer2 (see patch above) needs dlsym -> -ldl.
		args+=(--extra-libs=-ldl)
		;;
	macos)
		args+=(
			--enable-videotoolbox
			--enable-hwaccel=h264_videotoolbox,hevc_videotoolbox,av1_videotoolbox,vp9_videotoolbox
		)
		;;
	windows)
		args+=(
			--toolchain=msvc
			--target-os=win64
			--arch=x86_64
			# cl.exe defaults to the static CRT; everything else here
			# is /MD. See the meson_common comment above.
			--extra-cflags=-MD
			# Not every configure probe goes through pkg-config; the
			# fallbacks link a bare -lmbedtls with no search path and
			# fail with "cannot open input file mbedtls.lib".
			--extra-cflags=-I"${native_prefix}/include"
			--extra-ldflags=-libpath:"${native_prefix}/lib"
			--enable-d3d11va
			--enable-dxva2
			--enable-ffnvcodec
			--enable-cuda
			--enable-nvdec
			--enable-hwaccel=h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2,av1_d3d11va,av1_d3d11va2,vp9_d3d11va,vp9_d3d11va2,h264_dxva2,hevc_dxva2,av1_dxva2,vp9_dxva2,h264_nvdec,hevc_nvdec,av1_nvdec,vp9_nvdec
		)
		;;
	esac

	# configure prints only "X not found using pkg-config" on failure; the
	# actual compiler and linker invocation lives in config.log, which is
	# the only thing that distinguishes a missing library from one whose
	# name or link order the toolchain got wrong.
	if ! (cd "${ff}" && ./configure "${args[@]}"); then
		local cfglog="${ff}/ffbuild/config.log"
		echo
		# A tail alone is not enough. Autodetected libraries are probed
		# early and configure only dies about them in a sweep at the very
		# end ("$lib requested but not found"), so by the time it fails
		# the probe that actually explains it is a thousand lines above
		# the tail and nothing in the visible output names a cause.
		echo "---- probes for the libraries we require ----" >&2
		local l
		for l in zlib mbedtls libsrt librist ffnvcodec; do
			echo "== ${l} ==" >&2
			grep -n -B2 -A25 \
				-e "check_pkg_config ${l} " \
				-e "check_lib ${l} " \
				"${cfglog}" >&2 || echo "(no probe logged)" >&2
		done
		echo "---- tail of ffbuild/config.log ----" >&2
		tail -60 "${cfglog}" >&2 || true
		exit 1
	fi

	# config.mak marks a disabled component by prefixing it with '!'.
	local missing=()
	local component
	for component in ${FFMPEG_REQUIRED_CONFIG}; do
		if ! grep -qx "CONFIG_${component}=yes" "${ff}/ffbuild/config.mak"; then
			missing+=("${component}")
		fi
	done
	if [[ ${#missing[@]} -gt 0 ]]; then
		echo "FFmpeg configure did not enable: ${missing[*]}" >&2
		echo "Check the --enable-* component names in ${BASH_SOURCE[0]}." >&2
		exit 1
	fi

	local hwaccels
	hwaccels="$(grep -c '^CONFIG_[A-Z0-9_]*_HWACCEL=yes' "${ff}/ffbuild/config.mak" || true)"
	echo "FFmpeg configured with ${hwaccels} hwaccel(s)"
	if [[ ${hwaccels} -eq 0 ]]; then
		echo "no hardware accelerators enabled — hardware decode would be dead" >&2
		exit 1
	fi

	make -C "${ff}" -j"${jobs}"
	make -C "${ff}" install

	mark_built ffmpeg "${FFMPEG_VERSION}"
}

# ── Generated CMake description ──────────────────────────────────────────────
#
# The plugin's CMakeLists includes this file rather than rediscovering the
# stack. Our own libraries go in by absolute path (no -l name resolution to get
# wrong); everything else comes from pkg-config --static, which is the only
# thing that knows the full transitive system-library set for a static FFmpeg.
emit_cmake() {
	log "generating irl-deps.cmake"

	local libdir="${prefix}/lib"
	local out="${prefix}/irl-deps.cmake"

	# Link order matters for a single-pass static link.
	local ordered=(avformat avcodec swscale swresample avutil srt rist mbedtls mbedx509 mbedcrypto)

	# zlib is ours only on Windows; Linux and macOS pull the system one in
	# through pkg-config as -lz. Listing it explicitly guarantees it reaches
	# the link line: the Windows .pc chain does not surface it as a -l flag,
	# so it would otherwise be dropped and show up as unresolved inflate and
	# deflate at plugin link time. It goes last, after its consumers.
	if [[ ${host} == windows ]]; then
		ordered+=(z)
	fi

	local own=()
	local name path
	for name in "${ordered[@]}"; do
		path=""
		# .lib first so a Windows build picks the name the MSVC linker
		# expects: meson emits librist.a even under MSVC, and both that
		# and the rist.lib beside it would link, but mixing conventions
		# in one link line is needless room for surprise. On Linux only
		# the lib*.a form exists, so the order costs nothing there.
		for candidate in \
			"${libdir}/${name}.lib" \
			"${libdir}/lib${name}.lib" \
			"${libdir}/${name}_static.lib" \
			"${libdir}/lib${name}.a" \
			"${libdir}/lib${name}_static.a"; do
			[[ -f ${candidate} ]] && path="${candidate}" && break
		done
		if [[ -z ${path} ]]; then
			echo "missing static library: ${name} (looked in ${libdir})" >&2
			exit 1
		fi
		if [[ ${host} == windows ]]; then
			path="$(cygpath -m "${path}")"
		fi
		own+=("${path}")
	done

	# Everything pkg-config reports that is not one of ours is a system
	# dependency (-lm, -lva, -framework VideoToolbox, ws2_32.lib, ...).
	#
	# srt is queried explicitly: FFmpeg's .pc files do not propagate it, and
	# libsrt is C++, so its .pc is the only place the C++ runtime
	# (-lstdc++ / -lc++) appears.
	local raw
	raw="$(pkg-config --static --libs libavformat libavcodec libswscale libswresample libavutil srt librist 2>/dev/null || true)"

	local system=()
	local pending_framework=0
	local tok
	for tok in ${raw}; do
		if [[ ${pending_framework} -eq 1 ]]; then
			system+=("-Wl,-framework,${tok}")
			pending_framework=0
			continue
		fi
		case "${tok}" in
		-framework)
			pending_framework=1
			;;
		-L* | -Wl,-rpath* | -libpath:* | -LIBPATH:* | /libpath:* | /LIBPATH:*)
			# Our libraries are absolute paths; search paths that
			# point back into the prefix would only add ambiguity.
			#
			# The MSVC spellings must be matched here rather than
			# left to the -l* arm below, which is greedy enough to
			# read "-libpath:C:/x" as a library named "ibpath:C:/x"
			# and emit "ibpath:C:/x.lib".
			;;
		-l*)
			name="${tok#-l}"
			# Already listed by absolute path above.
			if [[ " ${ordered[*]} " == *" ${name} "* ]]; then
				continue
			fi
			# The compiler adds these itself, and naming them
			# explicitly only creates link-order hazards.
			case "${name}" in
			c | gcc | gcc_s) continue ;;
			esac
			# A prefix-local library we did not anticipate: keep it,
			# by absolute path, rather than silently dropping it.
			# .lib first, matching the candidate order above.
			if [[ -f "${libdir}/${name}.lib" ]]; then
				own+=("$(npath "${libdir}/${name}.lib")")
				continue
			elif [[ -f "${libdir}/lib${name}.a" ]]; then
				own+=("${libdir}/lib${name}.a")
				continue
			fi
			if [[ ${host} == windows ]]; then
				system+=("${name}.lib")
			else
				system+=("${tok}")
			fi
			;;
		*.lib)
			name="$(basename "${tok}")"
			[[ -f "${libdir}/${name}" ]] && continue
			system+=("${name}")
			;;
		-*)
			system+=("${tok}")
			;;
		esac
	done

	# Order within the system list does not matter (these resolve against
	# shared libraries), so collapse the duplicates pkg-config emits.
	local deduped=()
	for tok in "${system[@]:-}"; do
		[[ -z ${tok} ]] && continue
		[[ " ${deduped[*]:-} " == *" ${tok} "* ]] && continue
		deduped+=("${tok}")
	done
	# Not `system=("${deduped[@]:-}")`: on an empty array that idiom yields a
	# single empty element, and an empty entry in the CMake link list is an
	# error rather than a no-op.
	system=()
	if [[ ${#deduped[@]} -gt 0 ]]; then
		system=("${deduped[@]}")
	fi
	# Runtime-weak vaMapBuffer2 uses dlsym -> -ldl on Linux. FFmpeg's
	# --extra-libs=-ldl should surface via pkg-config, but if it doesn't
	# (static libs built before the patch, or pc files without Libs.private)
	# ensure -ldl is still on the link line so the final cdylib resolves
	# dlsym. Harmless if already present.
	if [[ ${host} == linux ]]; then
		if [[ ! " ${system[*]:-} " == *" -ldl "* ]]; then
			system+=("-ldl")
		fi
	fi

	{
		echo "# Generated by deps/build-deps.sh — do not edit."
		echo "set(IRL_DEPS_FOUND TRUE)"
		echo "set(IRL_DEPS_HOST \"${host}\")"
		echo "set(IRL_DEPS_FFMPEG_VERSION \"${FFMPEG_VERSION}\")"
		echo "set(IRL_DEPS_SRT_VERSION \"${SRT_VERSION}\")"
		echo "set(IRL_DEPS_LIBRIST_VERSION \"${LIBRIST_VERSION}\")"
		echo "set(IRL_DEPS_MBEDTLS_VERSION \"${MBEDTLS_VERSION}\")"
		echo "set(IRL_DEPS_INCLUDE_DIRS \"${native_prefix}/include\")"
		printf 'set(IRL_DEPS_STATIC_LIBRARIES\n'
		printf '    "%s"\n' "${own[@]}"
		printf ')\n'
		printf 'set(IRL_DEPS_SYSTEM_LIBRARIES\n'
		if [[ ${#system[@]} -gt 0 ]]; then
			printf '    "%s"\n' "${system[@]}"
		fi
		printf ')\n'
	} >"${out}"

	echo "wrote ${out}"
	cat "${out}"
	emit_env
}

# ── Generated environment file for the Rust build ───────────────────────────
#
# crates/ffmpeg/build.rs replays this. ffmpeg-sys-next links the five libav*
# archives itself (FFMPEG_DIR), so this lists only what they depend on: our
# own transitive static libraries as bare names in single-pass link order,
# system libraries as bare names, and macOS frameworks. Plain KEY=value,
# ';'-separated lists, absolute native paths, no quoting.
emit_env() {
	local out="${prefix}/irl-deps.env"
	local libdir="${native_prefix}/lib"

	local transitive_libs=() transitive_paths=() system_libs=() frameworks=()
	local p base name
	for p in "${own[@]}"; do
		base="$(basename "${p}")"
		name="${base%.*}"
		name="${name#lib}"
		name="${name%_static}"
		case "${name}" in
		avformat | avcodec | swscale | swresample | avutil) continue ;;
		esac
		transitive_libs+=("${name}")
		transitive_paths+=("${p}")
	done

	local tok
	for tok in "${system[@]:-}"; do
		[[ -z ${tok} ]] && continue
		case "${tok}" in
		-Wl,-framework,*) frameworks+=("${tok#-Wl,-framework,}") ;;
		-pthread | -lpthread) ;; # Rust's std links pthread already
		-l*) system_libs+=("${tok#-l}") ;;
		*.lib) system_libs+=("${tok%.lib}") ;;
		*) ;; # other linker flags are CMake-only
		esac
	done

	join() {
		local IFS=';'
		echo "$*"
	}

	{
		echo "# Generated by deps/build-deps.sh — do not edit."
		echo "IRL_DEPS_HOST=${host}"
		echo "IRL_DEPS_PREFIX=${native_prefix}"
		echo "IRL_DEPS_INCLUDE_DIR=${native_prefix}/include"
		echo "IRL_DEPS_LIBDIR=${libdir}"
		echo "IRL_DEPS_FFMPEG_VERSION=${FFMPEG_VERSION}"
		echo "IRL_DEPS_SRT_VERSION=${SRT_VERSION}"
		echo "IRL_DEPS_LIBRIST_VERSION=${LIBRIST_VERSION}"
		echo "IRL_DEPS_MBEDTLS_VERSION=${MBEDTLS_VERSION}"
		echo "IRL_DEPS_TRANSITIVE_LIBS=$(join "${transitive_libs[@]:-}")"
		echo "IRL_DEPS_TRANSITIVE_PATHS=$(join "${transitive_paths[@]:-}")"
		echo "IRL_DEPS_SYSTEM_LIBS=$(join "${system_libs[@]:-}")"
		echo "IRL_DEPS_FRAMEWORKS=$(join "${frameworks[@]:-}")"
	} >"${out}"

	echo "wrote ${out}"
	cat "${out}"
}

log "building bundled deps for ${host} into ${prefix}"
build_zlib
build_mbedtls
build_srt
build_librist
build_nvcodec
build_ffmpeg
emit_cmake
log "done"
