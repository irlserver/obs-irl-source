#!/usr/bin/env bash
#
# obs-irl-source — build a release archive from a cargo build artifact.
#
# Copyright (C) 2026 Thomas Lekanger
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Usage: scripts/package.sh <linux|windows|macos|deb> <artifact-dir> <out-dir> [version]
#
# <artifact-dir> holds the binary as cargo names it (libobs_irl_source.so,
# obs_irl_source.dll, libobs_irl_source.dylib); this script renames it and
# stages the platform's install layout, so an archive extracts straight into
# place with no further renaming. The version defaults to the workspace
# version in Cargo.toml, which is the single source of truth (release.yml
# checks the pushed tag against it).
#
# The locale file is not optional packaging: obs_module_text() falls back to
# the lookup key, so a build shipped without it renders the properties dialog
# as bare identifiers. THIRD_PARTY_NOTICES.md travels with the binary because
# the bundled stack includes LGPL FFmpeg, which wants its notices conveyed
# alongside the object code.

set -euo pipefail

die() {
	echo "package.sh: $*" >&2
	exit 1
}

[[ $# -ge 3 ]] || die "usage: package.sh <linux|windows|macos|deb> <artifact-dir> <out-dir> [version]"

platform="$1"
artifact_dir="$2"
out_dir="$3"
version="${4:-}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

[[ -d ${artifact_dir} ]] || die "no such artifact directory: ${artifact_dir}"
artifact_dir="$(cd "${artifact_dir}" && pwd)"

if [[ -z ${version} ]]; then
	# The workspace version, i.e. the `version = "x.y.z"` under
	# [workspace.package] and not one a member crate might pin.
	version="$(sed -nE '/^\[workspace\.package\]/,/^\[[^w]/ s/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' \
		"${repo_root}/Cargo.toml" | head -n1)"
fi
[[ -n ${version} ]] || die "could not read the version from Cargo.toml"

mkdir -p "${out_dir}"
out_dir="$(cd "${out_dir}" && pwd)"

staging="$(mktemp -d)"
trap 'rm -rf "${staging}"' EXIT

# The binary cargo produced, by its cargo name.
artifact() {
	local path="${artifact_dir}/$1"
	[[ -f ${path} ]] || die "expected ${1} in ${artifact_dir}"
	printf '%s' "${path}"
}

# locale/ + the two files every archive carries, into the data directory the
# platform's obs_module_file() resolves to.
stage_data() {
	mkdir -p "$1/locale"
	cp "${repo_root}/data/locale/en-US.ini" "$1/locale/"
	cp "${repo_root}/LICENSE" "${repo_root}/THIRD_PARTY_NOTICES.md" "$1/"
}

case "${platform}" in
linux)
	# Extracts into ~/.config/obs-studio/plugins/
	archive="${out_dir}/obs-irl-source-${version}-linux-x64.tar.gz"
	mkdir -p "${staging}/obs-irl-source/bin/64bit"
	cp "$(artifact libobs_irl_source.so)" \
		"${staging}/obs-irl-source/bin/64bit/obs-irl-source.so"
	stage_data "${staging}/obs-irl-source/data"
	tar -C "${staging}" -czf "${archive}" obs-irl-source
	;;
windows)
	# Extracts into the OBS Studio install folder; the DLL lands in
	# obs-plugins\64bit. No w32-pthreads.dll: the Rust plugin never calls
	# pthreads, which is what the C build needed it for.
	archive="${out_dir}/obs-irl-source-${version}-windows-x64.zip"
	mkdir -p "${staging}/obs-plugins/64bit"
	cp "$(artifact obs_irl_source.dll)" \
		"${staging}/obs-plugins/64bit/obs-irl-source.dll"
	stage_data "${staging}/data/obs-plugins/obs-irl-source"
	rm -f "${archive}"
	(cd "${staging}" && zip -qr "${archive}" obs-plugins data)
	;;
macos)
	# Extracts into ~/Library/Application Support/obs-studio/plugins/.
	# OBS on macOS only scans .plugin bundles — its module path is
	# %module%.plugin/Contents/MacOS with an extensionless binary — so this
	# must be a bundle, not the Linux-style bin/ directory.
	archive="${out_dir}/obs-irl-source-${version}-macos-arm64.zip"
	bundle="${staging}/obs-irl-source.plugin"
	mkdir -p "${bundle}/Contents/MacOS"
	cp "$(artifact libobs_irl_source.dylib)" "${bundle}/Contents/MacOS/obs-irl-source"
	sed "s/@PLUGIN_VERSION@/${version}/g" "${repo_root}/packaging/Info.plist.in" \
		>"${bundle}/Contents/Info.plist"
	# obs_module_file() resolves to Contents/Resources inside a bundle.
	stage_data "${bundle}/Contents/Resources"
	# On a Mac with an identity, sign (and optionally notarize) the
	# bundle. The release job assembles on Linux, where these are
	# silently skipped; a maintainer with credentials repackages on a
	# Mac and re-uploads. Notarization additionally needs
	# NOTARIZE_APPLE_ID / NOTARIZE_PASSWORD / NOTARIZE_TEAM_ID.
	if [[ $(uname -s) == Darwin ]]; then
		dsym="${out_dir}/obs-irl-source-${version}-macos-arm64.dSYM.tar.gz"
		if command -v dsymutil >/dev/null; then
			dsymutil "${bundle}/Contents/MacOS/obs-irl-source" \
				-o "${staging}/obs-irl-source.dSYM"
			tar -C "${staging}" -czf "${dsym}" obs-irl-source.dSYM
		fi
		if [[ -n ${CODESIGN_IDENTITY:-} ]]; then
			codesign --force --options runtime --timestamp \
				--sign "${CODESIGN_IDENTITY}" "${bundle}"
		fi
	fi
	rm -f "${archive}"
	(cd "${staging}" && zip -qr "${archive}" obs-irl-source.plugin)
	if [[ $(uname -s) == Darwin && -n ${CODESIGN_IDENTITY:-} && -n ${NOTARIZE_APPLE_ID:-} ]]; then
		xcrun notarytool submit "${archive}" --wait \
			--apple-id "${NOTARIZE_APPLE_ID}" \
			--password "${NOTARIZE_PASSWORD}" \
			--team-id "${NOTARIZE_TEAM_ID}"
	fi
	;;
deb)
	# A Debian package installing system-wide, next to distro OBS:
	# /usr/lib/<multiarch>/obs-plugins + /usr/share/obs/obs-plugins/<id>.
	command -v dpkg-deb >/dev/null || die "dpkg-deb not found"
	deb="${out_dir}/obs-irl-source_${version}_amd64.deb"
	pkgroot="${staging}/pkg"
	libdir="${pkgroot}/usr/lib/x86_64-linux-gnu/obs-plugins"
	datadir="${pkgroot}/usr/share/obs/obs-plugins/obs-irl-source"
	docdir="${pkgroot}/usr/share/doc/obs-irl-source"
	mkdir -p "${libdir}" "${datadir}" "${docdir}" "${pkgroot}/DEBIAN"
	cp "$(artifact libobs_irl_source.so)" "${libdir}/obs-irl-source.so"
	stage_data "${datadir}"
	# Debian wants licenses under /usr/share/doc; keep copies with the
	# plugin data too, matching the tarball layout.
	cp "${repo_root}/LICENSE" "${docdir}/copyright"
	cp "${repo_root}/THIRD_PARTY_NOTICES.md" "${docdir}/"
	cat >"${pkgroot}/DEBIAN/control" <<-EOF
		Package: obs-irl-source
		Version: ${version}
		Architecture: amd64
		Section: video
		Priority: optional
		Maintainer: Thomas Lekanger <datagutt@lekanger.no>
		Depends: obs-studio (>= 32.1)
		Homepage: https://github.com/irlserver/obs-irl-source
		Description: IRL streaming source plugin for OBS Studio
		 Receives live IRL streams over SRT, RTMP, RIST or any protocol the
		 bundled FFmpeg supports, with jitter buffering, PTS repair, adaptive
		 playback speed and hardware-accelerated decoding.
	EOF
	rm -f "${deb}"
	dpkg-deb --build --root-owner-group "${pkgroot}" "${deb}" >/dev/null
	archive="${deb}"
	;;
*)
	die "unknown platform '${platform}' (expected linux, windows, macos or deb)"
	;;
esac

echo "${archive}"
